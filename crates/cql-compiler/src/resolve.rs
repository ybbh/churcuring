//! Name resolution (doc/cql.md §3.1–3.7, §4.2, §4.3, A.3; pipeline §D.3).
//!
//! Consumes a surface [`Module`] and produces a [`ResolvedModule`]:
//!
//! - `Call` nodes naming the read/write primitives are rewritten in place:
//!   `read` → [`ExprKind::ReadPrim`], `insert`/`update`/`delete` →
//!   [`ExprKind::WriteCon`]; enum-variant calls become
//!   [`ExprKind::EnumConstruct`]. `lookup` stays a `Call` (expanded during
//!   desugaring) but is recorded as [`Callee::LookupPrim`].
//! - Every remaining `Call` and `Var` gets a side-table entry in
//!   [`Resolutions`], keyed by the callee/variable identifier span.
//!
//! Checks performed here: scoping & shadowing, undefined names, duplicate
//! module-level names, reserved effect-primitive names, lambda capture
//! discipline (§4.2), table-position rules (§4.3/A.3), `use` imports, and
//! named-argument validation against known signatures (A.3).

use std::collections::{HashMap, HashSet};

use miette::NamedSource;

use crate::ast::*;
use crate::diag::{CqlError, DiagBag};

/// The five reserved effect-primitive names (A.1): users may not declare
/// operators or bindings with these names.
pub const RESERVED_PRIMITIVES: &[&str] = &["read", "lookup", "insert", "update", "delete"];

/// Standard-library pure functions (doc/cql.md appendix B) with their
/// parameter names, for named-argument validation. `aggregate` (§4.8.3) is a
/// builtin combinator rather than a pure function but is called with named
/// arguments, so it is listed here too.
pub const STDLIB_SIGNATURES: &[(&str, &[&str])] = &[
    // string
    ("contains", &["s", "sub"]),
    ("starts_with", &["s", "pre"]),
    ("ends_with", &["s", "suf"]),
    ("length", &["s"]),
    ("concat", &["a", "b"]),
    ("to_string_int", &["x"]),
    ("to_string_float", &["x"]),
    ("to_string_date", &["d"]),
    ("to_string_bool", &["b"]),
    ("to_string_decimal", &["d"]),
    ("substring", &["s", "start", "length"]),
    ("trim", &["s"]),
    ("split", &["s", "sep"]),
    ("join", &["xs", "sep"]),
    // math
    ("abs", &["x"]),
    ("min", &["a", "b"]),
    ("max", &["a", "b"]),
    ("floor", &["x"]),
    ("ceil", &["x"]),
    ("round", &["x"]),
    // decimal
    ("decimal_from_string", &["s"]),
    ("round_to", &["d", "k"]),
    // date
    ("year", &["d"]),
    ("month", &["d"]),
    ("day", &["d"]),
    ("add_days", &["d", "n"]),
    ("days_between", &["a", "b"]),
    ("parse_date", &["s"]),
    ("day_of_week", &["d"]),
    // vector / iteration
    ("fold", &["xs", "init", "step"]),
    ("map", &["xs", "f"]),
    ("filter", &["xs", "p"]),
    ("append", &["xs", "x"]),
    ("to_vector", &["s"]),
    ("sort_by", &["xs", "key"]),
    ("take", &["xs", "n"]),
    ("drop", &["xs", "n"]),
    ("to_set", &["xs"]),
    ("is_empty", &["xs"]),
    ("concat_vector", &["a", "b"]),
    ("scan_left", &["xs", "init", "step"]),
    // set / bag
    ("size", &["s"]),
    ("the", &["s"]),
    ("only", &["s"]),
    ("union_all", &["s"]),
    ("bag_to_set", &["b"]),
    ("set_to_bag", &["s"]),
    ("copies_in", &["x", "b"]),
    ("bag_union", &["a", "b"]),
    // map
    ("map_get", &["m", "k"]),
    ("map_insert", &["m", "k", "v"]),
    ("map_remove", &["m", "k"]),
    ("map_keys", &["m"]),
    ("map_values", &["m"]),
    ("map_size", &["m"]),
    ("map_from_vector", &["pairs"]),
    ("map_to_vector", &["m"]),
    // option
    ("and_then", &["opt", "f"]),
    ("unwrap_or", &["opt", "default"]),
    ("is_some", &["opt"]),
    ("is_none", &["opt"]),
    // aggregate combinator + sugars (§4.8.3)
    ("aggregate", &["source", "group_key", "value", "reducer", "init", "finalize"]),
    ("count_by", &["src", "key"]),
    ("sum_by", &["src", "key", "val"]),
    ("avg_by", &["src", "key", "val"]),
    ("min_by", &["src", "key", "val"]),
    ("max_by", &["src", "key", "val"]),
];

/// Look up a standard-library signature by name.
pub fn stdlib_signature(name: &str) -> Option<&'static [&'static str]> {
    STDLIB_SIGNATURES.iter().find(|(n, _)| *n == name).map(|(_, sig)| *sig)
}

/// What a `Call` node resolved to (kept as a side table keyed by the callee
/// name span).
#[derive(Debug, Clone, PartialEq)]
pub enum Callee {
    /// A local binding used as a function value (positional args only).
    LocalValue,
    /// A module-level `const` used as a function value.
    GlobalValue,
    /// A declared operator (`function`/`query`/`action`), possibly external
    /// or imported. `module_local` distinguishes same-module operators.
    Operator { name: String, level: EffectLevel, module_local: bool },
    /// A standard-library pure function (or the `aggregate` combinator).
    StdLib { name: String },
    /// The `lookup` read primitive; the `Call` node is kept and expanded
    /// during desugaring.
    LookupPrim,
}

/// What a `Var` node resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum VarRes {
    /// A local binding (let/lambda param/operator param/pattern/generator).
    Local,
    /// A module-level (or imported) constant.
    Const,
    /// A `function` name used as a first-class L0 value.
    Function,
    /// A standard-library function used as a value.
    StdLibFn,
    /// A table name in a legal table position (generator/quantifier source);
    /// the desugarer expands the table-name sugar.
    TableSugar,
}

/// Side tables produced by name resolution, keyed by identifier spans.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolutions {
    /// Resolution of every `Call` node that was kept (not rewritten).
    pub callee: HashMap<Span, Callee>,
    /// Resolution of every `Var` node.
    pub vars: HashMap<Span, VarRes>,
    /// Names of all enums visible in this module (declared + imported),
    /// used by the termination pass to recognize inductive types.
    pub known_enums: HashSet<String>,
}

/// The output of name resolution: the rewritten module plus side tables.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModule {
    pub module: Module,
    pub resolved: Resolutions,
}

/// A module imported via `use`, as supplied by the driver from the imported
/// module's own compilation results.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedModule {
    pub path: Vec<String>,
    pub public_items: Vec<ImportedItem>,
}

/// A single public item of an imported module. `params` carries the
/// parameter names of operators (for named-argument validation); the full
/// type information is filled in by later passes.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedItem {
    pub name: String,
    pub kind: ImportedKind,
    pub params: Option<Vec<String>>,
}

/// The kind of a public item imported from a dependency module.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportedKind {
    Function,
    Query,
    Action,
    Const,
    /// An enum variant constructor; `arity` is its number of payload arguments.
    EnumVariant { arity: usize },
    Enum,
    TypeAlias,
}

/// Resolve a module with a placeholder source for diagnostics.
pub fn resolve_module(
    module: Module,
    imports: &[ImportedModule],
) -> Result<ResolvedModule, DiagBag> {
    let src = NamedSource::new(format!("{}.cql", module.name.node), String::new());
    resolve_module_with_src(module, imports, src)
}

/// Resolve a module, using `src` as the source text attached to diagnostics.
pub fn resolve_module_with_src(
    mut module: Module,
    imports: &[ImportedModule],
    src: NamedSource<String>,
) -> Result<ResolvedModule, DiagBag> {
    let mut r = Resolver::new(src);
    r.collect_globals(&module, imports);
    r.resolve_items(&mut module.items);
    let resolved = r.resolutions;
    r.diags.into_result(ResolvedModule { module, resolved })
}

#[derive(Debug, Clone)]
enum GlobalKind {
    Operator { level: EffectLevel, params: Vec<String>, module_local: bool },
    Const,
    Variant { arity: usize },
    Table,
    /// Type alias / enum / index / invariant / test / property / fairness
    /// name: legal in its own position, never a value.
    NonValue(&'static str),
}

/// A lambda frame: records the scope depth at lambda entry, the declared
/// captures, and the captures actually used (with first-use span).
struct LambdaFrame {
    base: usize,
    declared: Vec<Ident>,
    used: HashMap<String, Span>,
}

struct Resolver {
    diags: DiagBag,
    src: NamedSource<String>,
    globals: HashMap<String, GlobalKind>,
    /// Local scopes; shadowing is allowed, so each scope is a set of names.
    scopes: Vec<HashSet<String>>,
    lambda_frames: Vec<LambdaFrame>,
    /// In-scope generic type parameters (for type-name checking).
    type_params: Vec<HashSet<String>>,
    resolutions: Resolutions,
}

impl Resolver {
    fn new(src: NamedSource<String>) -> Self {
        Resolver {
            diags: DiagBag::new(),
            src,
            globals: HashMap::new(),
            scopes: Vec::new(),
            lambda_frames: Vec::new(),
            type_params: Vec::new(),
            resolutions: Resolutions::default(),
        }
    }

    fn err(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_error(CqlError::new(self.src.clone(), span, message, help));
    }

    fn warn(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_warning(CqlError::new(self.src.clone(), span, message, help));
    }

    // ---- global collection -------------------------------------------------

    fn collect_globals(&mut self, module: &Module, imports: &[ImportedModule]) {
        for item in &module.items {
            self.declare_global(item);
        }
        // Imported enums are visible for inductive-type recognition.
        for im in imports {
            for it in &im.public_items {
                if matches!(it.kind, ImportedKind::Enum) {
                    self.resolutions.known_enums.insert(it.name.clone());
                }
            }
        }
        for item in &module.items {
            if let Item::Use(u) = item {
                self.process_use(u, imports);
            }
        }
    }

    fn insert_global(&mut self, name: &Ident, kind: GlobalKind) {
        if RESERVED_PRIMITIVES.contains(&name.node.as_str()) {
            self.err(
                name.span,
                format!("`{}` is a reserved effect-primitive name and cannot be declared", name.node),
                Some("choose a different name".to_string()),
            );
            return;
        }
        if self.globals.contains_key(&name.node) {
            self.err(name.span, format!("duplicate module-level name `{}`", name.node), None);
            return;
        }
        self.globals.insert(name.node.clone(), kind);
    }

    fn declare_global(&mut self, item: &Item) {
        match item {
            Item::Use(_) => {}
            Item::Const(c) => {
                self.insert_global(&c.name, GlobalKind::Const);
            }
            Item::TypeAlias(t) => {
                self.insert_global(&t.name, GlobalKind::NonValue("type alias"));
            }
            Item::Enum(e) => {
                self.insert_global(&e.name, GlobalKind::NonValue("enum"));
                self.resolutions.known_enums.insert(e.name.node.clone());
                for v in &e.variants {
                    let arity = match &v.payload {
                        VariantPayload::None => 0,
                        VariantPayload::Tuple(ts) => ts.len(),
                        VariantPayload::Record(_) => 1,
                    };
                    self.insert_global(&v.name, GlobalKind::Variant { arity });
                }
            }
            Item::Table(t) => {
                self.insert_global(&t.name, GlobalKind::Table);
            }
            Item::Index(i) => {
                self.insert_global(&i.name, GlobalKind::NonValue("index"));
            }
            Item::Operator(o) => {
                let params = o.params.iter().map(|p| p.name.node.clone()).collect();
                self.insert_global(
                    &o.name,
                    GlobalKind::Operator { level: o.level, params, module_local: true },
                );
            }
            Item::Invariant(i) => {
                self.insert_global(&i.name, GlobalKind::NonValue("invariant"));
            }
            Item::Test(t) => {
                self.insert_global(&t.name, GlobalKind::NonValue("test"));
            }
            Item::Property(p) => {
                self.insert_global(&p.name, GlobalKind::NonValue("property"));
            }
            Item::Fairness(_) => {}
        }
    }

    fn process_use(&mut self, u: &UseDecl, imports: &[ImportedModule]) {
        let path: Vec<String> = u.path.iter().map(|i| i.node.clone()).collect();
        let found = imports.iter().find(|im| im.path == path);
        let Some(im) = found else {
            let span = u.path.first().map(|i| i.span).unwrap_or(Span::new_dummy());
            self.err(
                span,
                format!("unresolved import `{}`", path.join("::")),
                Some("the module is not among the compilation's resolved imports".to_string()),
            );
            return;
        };
        if let Some(alias) = &u.alias {
            self.warn(
                alias.span,
                format!("module alias `{}` is reserved for future qualified access", alias.node),
                Some("items are imported unqualified for now".to_string()),
            );
        }
        for it in &im.public_items {
            let kind = match &it.kind {
                ImportedKind::Function => GlobalKind::Operator {
                    level: EffectLevel::Function,
                    params: it.params.clone().unwrap_or_default(),
                    module_local: false,
                },
                ImportedKind::Query => GlobalKind::Operator {
                    level: EffectLevel::Query,
                    params: it.params.clone().unwrap_or_default(),
                    module_local: false,
                },
                ImportedKind::Action => GlobalKind::Operator {
                    level: EffectLevel::Action,
                    params: it.params.clone().unwrap_or_default(),
                    module_local: false,
                },
                ImportedKind::Const => GlobalKind::Const,
                ImportedKind::EnumVariant { arity } => GlobalKind::Variant { arity: *arity },
                ImportedKind::Enum => GlobalKind::NonValue("enum"),
                ImportedKind::TypeAlias => GlobalKind::NonValue("type alias"),
            };
            let span = u.path.last().map(|i| i.span).unwrap_or(Span::new_dummy());
            if self.globals.contains_key(&it.name) {
                self.err(
                    span,
                    format!("imported name `{}` conflicts with an existing module-level name", it.name),
                    None,
                );
            } else {
                self.globals.insert(it.name.clone(), kind);
            }
        }
    }

    // ---- items -------------------------------------------------------------

    fn resolve_items(&mut self, items: &mut [Item]) {
        for item in items {
            match item {
                Item::Use(_) => {}
                Item::Const(c) => {
                    self.check_type(&c.ty);
                    self.expr(&mut c.value);
                }
                Item::TypeAlias(t) => {
                    self.type_params.push(t.params.iter().map(|p| p.node.clone()).collect());
                    self.check_type(&t.ty);
                    self.type_params.pop();
                }
                Item::Enum(e) => {
                    self.type_params.push(e.params.iter().map(|p| p.node.clone()).collect());
                    for v in &e.variants {
                        match &v.payload {
                            VariantPayload::None => {}
                            VariantPayload::Tuple(ts) => {
                                for t in ts {
                                    self.check_type(t);
                                }
                            }
                            VariantPayload::Record(fields) => {
                                for (_, t) in fields {
                                    self.check_type(t);
                                }
                            }
                        }
                    }
                    self.type_params.pop();
                }
                Item::Table(t) => {
                    for (_, ty) in &t.fields {
                        self.check_type(ty);
                    }
                    for fk in &t.fks {
                        if !matches!(self.globals.get(&fk.references.node), Some(GlobalKind::Table))
                        {
                            self.err(
                                fk.references.span,
                                format!("foreign key references unknown table `{}`", fk.references.node),
                                None,
                            );
                        }
                    }
                }
                Item::Index(i) => {
                    if !matches!(self.globals.get(&i.table.node), Some(GlobalKind::Table)) {
                        self.err(
                            i.table.span,
                            format!("index declared on unknown table `{}`", i.table.node),
                            None,
                        );
                    }
                }
                Item::Operator(o) => {
                    self.type_params.push(o.type_params.iter().map(|p| p.node.clone()).collect());
                    for p in &o.params {
                        self.check_type(&p.ty);
                    }
                    self.check_type(&o.ret);
                    if let Some(body) = &mut o.body {
                        self.scopes.push(HashSet::new());
                        for p in &o.params {
                            self.introduce_binding(&p.name);
                        }
                        self.expr(body);
                        self.scopes.pop();
                    }
                    self.type_params.pop();
                }
                Item::Invariant(i) => {
                    if !matches!(self.globals.get(&i.table.node), Some(GlobalKind::Table)) {
                        self.err(
                            i.table.span,
                            format!("invariant declared on unknown table `{}`", i.table.node),
                            None,
                        );
                    }
                    self.expr(&mut i.body);
                }
                Item::Test(t) => {
                    for stmt in &mut t.stmts {
                        match stmt {
                            TestStmt::Fixture { table, rows } => {
                                if !matches!(self.globals.get(&table.node), Some(GlobalKind::Table))
                                {
                                    self.err(
                                        table.span,
                                        format!("fixture for unknown table `{}`", table.node),
                                        None,
                                    );
                                }
                                self.expr(rows);
                            }
                            TestStmt::Expect { lhs, rhs } => {
                                self.expr(lhs);
                                self.expr(rhs);
                            }
                        }
                    }
                }
                Item::Property(p) => {
                    self.temporal(&mut p.body);
                }
                Item::Fairness(f) => {
                    for a in &f.actions {
                        if !matches!(
                            self.globals.get(&a.node),
                            Some(GlobalKind::Operator { .. })
                        ) {
                            self.err(
                                a.span,
                                format!("fairness references unknown operator `{}`", a.node),
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    fn temporal(&mut self, t: &mut TemporalExpr) {
        match t {
            TemporalExpr::Always(inner) | TemporalExpr::Eventually(inner) => self.temporal(inner),
            TemporalExpr::LeadsTo { lhs, rhs } | TemporalExpr::Until { lhs, rhs } => {
                self.temporal(lhs);
                self.temporal(rhs);
            }
            TemporalExpr::Primed(e) | TemporalExpr::State(e) => self.expr(e),
        }
    }

    // ---- locals & captures ---------------------------------------------------

    /// Introduce a local binding into the innermost scope, checking the
    /// reserved primitive names.
    fn introduce_binding(&mut self, name: &Ident) {
        if RESERVED_PRIMITIVES.contains(&name.node.as_str()) {
            self.err(
                name.span,
                format!("`{}` is a reserved effect-primitive name and cannot be bound", name.node),
                Some("choose a different name".to_string()),
            );
            return;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.node.clone());
        }
    }

    fn introduce_pattern(&mut self, pat: &Pattern) {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Lit(_) | PatternKind::None
            | PatternKind::ConsNil => {}
            PatternKind::Bind(name) => self.introduce_binding(name),
            PatternKind::Some(inner) => self.introduce_pattern(inner),
            PatternKind::Variant { name, args } => {
                self.check_variant_pattern(name);
                for p in args {
                    self.introduce_pattern(p);
                }
            }
            PatternKind::Tuple(pats) => {
                for p in pats {
                    self.introduce_pattern(p);
                }
            }
            PatternKind::Record(names) => {
                for n in names {
                    self.introduce_binding(n);
                }
            }
            PatternKind::Cons { head, tail } => {
                self.introduce_pattern(head);
                self.introduce_pattern(tail);
            }
        }
    }

    fn check_variant_pattern(&mut self, name: &Ident) {
        match self.globals.get(&name.node) {
            Some(GlobalKind::Variant { .. }) => {}
            _ => self.err(
                name.span,
                format!("unknown enum variant `{}` in pattern", name.node),
                None,
            ),
        }
    }

    /// Find a local binding's scope index (innermost scope = last index).
    fn find_local(&self, name: &str) -> Option<usize> {
        self.scopes.iter().rposition(|s| s.contains(name))
    }

    /// Record a use of a local binding for lambda-capture checking: if the
    /// binding was introduced outside the innermost lambda, that lambda must
    /// declare it as a capture.
    fn note_local_use(&mut self, name: &str, scope_idx: usize, span: Span) {
        if let Some(frame) = self.lambda_frames.last_mut() {
            if scope_idx < frame.base {
                frame.used.entry(name.to_string()).or_insert(span);
            }
        }
    }

    // ---- expressions ---------------------------------------------------------

    fn expr(&mut self, e: &mut Expr) {
        match &mut e.kind {
            ExprKind::Lit(_) | ExprKind::OptionNone => {}
            ExprKind::Var(_) => {
                let name = match &e.kind {
                    ExprKind::Var(n) => n.clone(),
                    _ => unreachable!(),
                };
                self.resolve_var(e, name);
            }
            ExprKind::Block { lets, tail } => {
                self.scopes.push(HashSet::new());
                for l in lets {
                    self.expr(&mut l.value);
                    if let Some(ty) = &l.ty {
                        self.check_type(ty);
                    }
                    self.introduce_pattern(&l.pat);
                }
                self.expr(tail);
                self.scopes.pop();
            }
            ExprKind::Let { pat, value, body } => {
                self.expr(value);
                self.scopes.push(HashSet::new());
                self.introduce_pattern(pat);
                self.expr(body);
                self.scopes.pop();
            }
            ExprKind::Lambda(l) => self.lambda(l),
            ExprKind::App { func, args } => {
                self.expr(func);
                for a in args {
                    if let Some(n) = &a.name {
                        self.err(
                            n.span,
                            "named arguments are not allowed in function-value calls",
                            Some("only operator and standard-library calls accept named arguments".to_string()),
                        );
                    }
                    self.expr(&mut a.value);
                }
            }
            ExprKind::Call(_) => self.resolve_call(e),
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.scopes.push(HashSet::new());
                    self.introduce_pattern(&arm.pat);
                    self.expr(&mut arm.body);
                    self.scopes.pop();
                }
            }
            ExprKind::If { cond, then_br, else_br } => {
                self.expr(cond);
                self.expr(then_br);
                self.expr(else_br);
            }
            ExprKind::Try(inner) => self.expr(inner),
            ExprKind::RecordLit { fields } => {
                for f in fields {
                    self.expr(&mut f.value);
                }
            }
            ExprKind::RecordUpd { base, fields } => {
                self.expr(base);
                for f in fields {
                    self.expr(&mut f.value);
                }
            }
            ExprKind::Tuple(items) | ExprKind::Vector(items) | ExprKind::SetLiteral(items)
            | ExprKind::BagLiteral(items) => {
                for item in items {
                    self.expr(item);
                }
            }
            ExprKind::SetFilter { pat, source, pred } => {
                self.generator_source(source);
                self.scopes.push(HashSet::new());
                self.introduce_pattern(pat);
                self.expr(pred);
                self.scopes.pop();
            }
            ExprKind::SetMap { elem, gens } | ExprKind::BagMap { elem, gens } => {
                self.scopes.push(HashSet::new());
                for g in gens {
                    self.generator_source(&mut g.source);
                    self.introduce_pattern(&g.pat);
                }
                self.expr(elem);
                self.scopes.pop();
            }
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::OptionSome(inner) => self.expr(inner),
            ExprKind::StrInterp(parts) => {
                for p in parts {
                    if let StrPart::Interp(inner) = p {
                        self.expr(inner);
                    }
                }
            }
            ExprKind::Quantifier { gens, body, .. } => {
                self.scopes.push(HashSet::new());
                for g in gens {
                    self.generator_source(&mut g.source);
                    self.introduce_pattern(&g.pat);
                }
                self.expr(body);
                self.scopes.pop();
            }
            ExprKind::Cast { expr, ty } => {
                self.expr(expr);
                self.check_type(ty);
            }
            ExprKind::BinOp { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::UnOp { operand, .. } => self.expr(operand),
            ExprKind::Primed(inner) => self.expr(inner),
            ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => self.expr(base),
            ExprKind::MethodCall { recv, args, .. } => {
                self.expr(recv);
                for a in args {
                    // Method-call sugar dispatches during type checking;
                    // named-argument validation is deferred to that pass.
                    self.expr(&mut a.value);
                }
            }
            // Resolved nodes are produced by this pass; if present (e.g. in
            // re-resolution) just recurse into children.
            ExprKind::ReadPrim { predicate, .. } => self.expr(predicate),
            ExprKind::WriteCon(w) => match w {
                WriteCon::Insert { row, .. } => self.expr(row),
                WriteCon::Update { key, transform, .. } => {
                    self.expr(key);
                    self.expr(transform);
                }
                WriteCon::Delete { key, .. } => self.expr(key),
            },
            ExprKind::EnumConstruct { args, .. } => {
                for a in args {
                    self.expr(a);
                }
            }
        }
    }

    /// Resolve a generator/quantifier source: a bare table name here is the
    /// legal table-name sugar (§4.3).
    fn generator_source(&mut self, source: &mut Expr) {
        if self.try_table_sugar(source) {
            return;
        }
        self.expr(source);
    }

    /// If `e` is a bare table name, record the table-name sugar resolution
    /// ([`VarRes::TableSugar`]) and return true.
    fn try_table_sugar(&mut self, e: &mut Expr) -> bool {
        if let ExprKind::Var(name) = &e.kind {
            if matches!(self.globals.get(&name.node), Some(GlobalKind::Table)) {
                self.resolutions.vars.insert(name.span, VarRes::TableSugar);
                return true;
            }
        }
        false
    }

    fn resolve_var(&mut self, e: &mut Expr, name: Ident) {
        if let Some(idx) = self.find_local(&name.node) {
            self.note_local_use(&name.node, idx, name.span);
            self.resolutions.vars.insert(name.span, VarRes::Local);
            return;
        }
        match self.globals.get(&name.node) {
            Some(GlobalKind::Const) => {
                self.resolutions.vars.insert(name.span, VarRes::Const);
            }
            Some(GlobalKind::Operator { level: EffectLevel::Function, .. }) => {
                self.resolutions.vars.insert(name.span, VarRes::Function);
            }
            Some(GlobalKind::Operator { level, .. }) => {
                let lvl = match level {
                    EffectLevel::Query => "query",
                    EffectLevel::Action => "action",
                    EffectLevel::Function => unreachable!(),
                };
                self.err(
                    name.span,
                    format!("{} name `{}` cannot be used as a value", lvl, name.node),
                    Some("only `function` names are first-class values (§3.7)".to_string()),
                );
            }
            Some(GlobalKind::Variant { arity: 0 }) => {
                e.kind = ExprKind::EnumConstruct { name, args: vec![] };
            }
            Some(GlobalKind::Variant { .. }) => {
                self.err(
                    name.span,
                    format!("enum variant `{}` has a payload and must be called with arguments", name.node),
                    None,
                );
            }
            Some(GlobalKind::Table) => {
                self.err(
                    name.span,
                    format!("table `{}` is only allowed in table position", name.node),
                    Some("table positions: first argument of `read`/`lookup`, or a generator/quantifier source (§4.3)".to_string()),
                );
            }
            Some(GlobalKind::NonValue(kind)) => {
                self.err(
                    name.span,
                    format!("{} name `{}` is not a value", kind, name.node),
                    None,
                );
            }
            None => {
                if stdlib_signature(&name.node).is_some() {
                    self.resolutions.vars.insert(name.span, VarRes::StdLibFn);
                } else {
                    self.err(name.span, format!("undefined name `{}`", name.node), None);
                }
            }
        }
    }

    fn resolve_call(&mut self, e: &mut Expr) {
        let ExprKind::Call(call) = &e.kind else { unreachable!() };
        let name = call.name.node.clone();
        let span = call.name.span;

        // Reserved effect primitives (recognized by name, A.1).
        match name.as_str() {
            "read" => {
                self.rewrite_read(e);
                return;
            }
            "lookup" => {
                self.check_lookup(e);
                return;
            }
            "insert" | "update" | "delete" => {
                self.rewrite_write(e, &name);
                return;
            }
            _ => {}
        }

        // Local binding shadows everything (including stdlib, A.3).
        if let Some(idx) = self.find_local(&name) {
            self.note_local_use(&name, idx, span);
            let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
            self.reject_named_args(call, "function-value calls");
            for a in &mut call.args {
                self.expr(&mut a.value);
            }
            self.resolutions.callee.insert(span, Callee::LocalValue);
            return;
        }

        match self.globals.get(&name).cloned() {
            Some(GlobalKind::Operator { level, params, module_local }) => {
                let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
                let param_refs: Vec<&str> = params.iter().map(|s| s.as_str()).collect();
                self.check_named_args(&name, &param_refs, &call.args, span);
                for a in &mut call.args {
                    self.expr(&mut a.value);
                }
                self.resolutions.callee.insert(span, Callee::Operator { name, level, module_local });
            }
            Some(GlobalKind::Variant { arity }) => {
                let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
                self.reject_named_args(call, "enum-variant constructions");
                if call.args.len() != arity {
                    self.err(
                        span,
                        format!("variant `{}` takes {} argument(s), got {}", name, arity, call.args.len()),
                        None,
                    );
                }
                let mut args: Vec<Expr> = call.args.drain(..).map(|a| a.value).collect();
                for a in &mut args {
                    self.expr(a);
                }
                e.kind = ExprKind::EnumConstruct { name: call.name.clone(), args };
            }
            Some(GlobalKind::Const) => {
                let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
                self.reject_named_args(call, "calls of a constant");
                for a in &mut call.args {
                    self.expr(&mut a.value);
                }
                self.resolutions.callee.insert(span, Callee::GlobalValue);
            }
            Some(GlobalKind::Table) => {
                self.err(span, format!("table `{}` is not callable", name), None);
            }
            Some(GlobalKind::NonValue(kind)) => {
                self.err(span, format!("{} name `{}` is not callable", kind, name), None);
            }
            None => {
                if let Some(sig) = stdlib_signature(&name) {
                    let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
                    self.check_named_args(&name, sig, &call.args, span);
                    for (i, a) in call.args.iter_mut().enumerate() {
                        // §4.8.2: `to_vector(t)` on a bare table name is a legal
                        // table position — the table-name sugar (read all rows,
                        // then materialize), as in `fold(to_vector(t), ...)`.
                        if i == 0 && name == "to_vector" && self.try_table_sugar(&mut a.value) {
                            continue;
                        }
                        self.expr(&mut a.value);
                    }
                    self.resolutions.callee.insert(span, Callee::StdLib { name });
                } else {
                    self.err(span, format!("undefined name `{}`", name), None);
                    let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
                    for a in &mut call.args {
                        self.expr(&mut a.value);
                    }
                }
            }
        }
    }

    /// Resolve the first argument of a primitive, which must be a table name
    /// declared in this module (tables are not importable, §3.1).
    fn table_arg(&mut self, call: &mut Call, prim: &str) -> Option<Ident> {
        let first = call.args.first_mut()?;
        let value = &mut first.value;
        match &value.kind {
            ExprKind::Var(t) => match self.globals.get(&t.node) {
                Some(GlobalKind::Table) => Some(t.clone()),
                _ => {
                    self.err(
                        t.span,
                        format!("first argument of `{}` must be a table declared in this module", prim),
                        None,
                    );
                    None
                }
            },
            _ => {
                self.err(
                    value.span,
                    format!("first argument of `{}` must be a table name", prim),
                    None,
                );
                self.expr(value);
                None
            }
        }
    }

    fn reject_named_args(&mut self, call: &Call, what: &str) {
        for a in &call.args {
            if let Some(n) = &a.name {
                self.err(
                    n.span,
                    format!("named arguments are not allowed in {}", what),
                    Some("only operator and standard-library calls accept named arguments".to_string()),
                );
            }
        }
    }

    /// Positional-only check for primitives, plus exact arity.
    fn check_primitive_args(&mut self, call: &Call, prim: &str, arity: usize) {
        self.reject_named_args(call, "effect-primitive calls");
        if call.args.len() != arity {
            self.err(
                call.name.span,
                format!("`{}` takes {} argument(s), got {}", prim, arity, call.args.len()),
                None,
            );
        }
    }

    fn rewrite_read(&mut self, e: &mut Expr) {
        let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
        self.check_primitive_args(call, "read", 2);
        let table = self.table_arg(call, "read");
        for a in call.args.iter_mut().skip(1) {
            self.expr(&mut a.value);
        }
        if let (Some(table), Some(pred)) = (table, call.args.get(1)) {
            let pred = pred.value.clone();
            e.kind = ExprKind::ReadPrim { table, predicate: Box::new(pred) };
        }
        // On error the `Call` node is left in place; the pass fails anyway.
    }

    fn check_lookup(&mut self, e: &mut Expr) {
        let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
        self.check_primitive_args(call, "lookup", 2);
        let span = call.name.span;
        let table_ok = self.table_arg(call, "lookup").is_some();
        for a in call.args.iter_mut().skip(1) {
            self.expr(&mut a.value);
        }
        if table_ok {
            self.resolutions.callee.insert(span, Callee::LookupPrim);
        }
    }

    fn rewrite_write(&mut self, e: &mut Expr, prim: &str) {
        let arity = match prim {
            "insert" | "delete" => 2,
            _ => 3, // update
        };
        let ExprKind::Call(call) = &mut e.kind else { unreachable!() };
        self.check_primitive_args(call, prim, arity);
        let table = self.table_arg(call, prim);
        for a in call.args.iter_mut().skip(1) {
            self.expr(&mut a.value);
        }
        let con = match (prim, table, call.args.len()) {
            ("insert", Some(table), 2) => Some(WriteCon::Insert {
                table,
                row: Box::new(call.args[1].value.clone()),
            }),
            ("update", Some(table), 3) => Some(WriteCon::Update {
                table,
                key: Box::new(call.args[1].value.clone()),
                transform: Box::new(call.args[2].value.clone()),
            }),
            ("delete", Some(table), 2) => Some(WriteCon::Delete {
                table,
                key: Box::new(call.args[1].value.clone()),
            }),
            _ => None,
        };
        if let Some(con) = con {
            e.kind = ExprKind::WriteCon(con);
        }
    }

    // ---- named arguments (A.3) -----------------------------------------------

    /// Validate a call's arguments against a known parameter list: named
    /// arguments must follow all positional ones, match parameter names,
    /// avoid duplicates, and cover every parameter (no defaults).
    fn check_named_args(&mut self, callee: &str, params: &[&str], args: &[Arg], call_span: Span) {
        let mut filled: Vec<Option<Span>> = vec![None; params.len()];
        let mut seen_named = false;
        for (i, a) in args.iter().enumerate() {
            match &a.name {
                None => {
                    if seen_named {
                        self.err(
                            a.value.span,
                            "positional argument follows a named argument",
                            Some("named arguments must come after all positional arguments".to_string()),
                        );
                    }
                    if i >= params.len() {
                        self.err(
                            a.value.span,
                            format!("too many arguments to `{}` (expected {})", callee, params.len()),
                            None,
                        );
                    } else {
                        filled[i] = Some(a.value.span);
                    }
                }
                Some(n) => {
                    seen_named = true;
                    match params.iter().position(|p| *p == n.node) {
                        None => self.err(
                            n.span,
                            format!("unknown argument name `{}` for `{}`", n.node, callee),
                            None,
                        ),
                        Some(p) => {
                            if filled[p].is_some() {
                                self.err(
                                    n.span,
                                    format!("argument `{}` is passed more than once", n.node),
                                    None,
                                );
                            } else {
                                filled[p] = Some(n.span);
                            }
                        }
                    }
                }
            }
        }
        if args.len() <= params.len() {
            let missing: Vec<&str> = params
                .iter()
                .enumerate()
                .filter(|(i, _)| filled[*i].is_none())
                .map(|(_, p)| *p)
                .collect();
            if !missing.is_empty() {
                self.err(
                    call_span,
                    format!("missing argument(s) {} in call to `{}`", missing.join(", "), callee),
                    Some("CQL has no default arguments; every parameter must be passed".to_string()),
                );
            }
        }
    }

    // ---- lambdas (§4.2) ---------------------------------------------------------

    fn lambda(&mut self, l: &mut Lambda) {
        // Declared captures must name visible outer local bindings; resolving
        // them here also propagates the capture requirement to any outer
        // lambda (nested-lambda rule, §4.2).
        for cap in &l.captures {
            match self.find_local(&cap.node) {
                Some(idx) => self.note_local_use(&cap.node, idx, cap.span),
                None => self.err(
                    cap.span,
                    format!("capture `{}` does not name an outer local binding", cap.node),
                    Some("only let bindings, lambda/operator parameters and generator bindings can be captured".to_string()),
                ),
            }
        }
        let frame = LambdaFrame {
            base: self.scopes.len(),
            declared: l.captures.clone(),
            used: HashMap::new(),
        };
        self.lambda_frames.push(frame);
        self.scopes.push(HashSet::new());
        for p in &mut l.params {
            if let Some(ty) = &p.ty {
                self.check_type(ty);
            }
            self.introduce_pattern(&p.pat);
        }
        if let Some(ret) = &l.ret {
            self.check_type(ret);
        }
        self.expr(&mut l.body);
        self.scopes.pop();
        let frame = self.lambda_frames.pop().expect("lambda frame stack balanced");

        // Every used outer binding must be declared; every declared capture
        // must be used.
        for (name, use_span) in &frame.used {
            if !frame.declared.iter().any(|c| c.node == *name) {
                self.err(
                    *use_span,
                    format!("outer binding `{}` is used but not listed in the lambda captures", name),
                    Some(format!("add `{}` to the capture list `[...]`", name)),
                );
            }
        }
        for cap in &frame.declared {
            if !frame.used.contains_key(&cap.node) {
                self.warn(
                    cap.span,
                    format!("capture `{}` is declared but never used", cap.node),
                    None,
                );
            }
        }
    }

    // ---- type names -------------------------------------------------------------

    /// Light type-name validation (full type checking happens later): named
    /// types must be visible, `key t`/`value t` must name a module table.
    fn check_type(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Bool
            | TypeKind::Int
            | TypeKind::Float
            | TypeKind::Decimal(_)
            | TypeKind::String
            | TypeKind::Date => {}
            TypeKind::Named { name, args } => {
                let is_type_param = self.type_params.iter().rev().any(|s| s.contains(&name.node));
                let is_known = name.node == "write_op"
                    || matches!(
                        self.globals.get(&name.node),
                        Some(GlobalKind::NonValue("type alias"))
                            | Some(GlobalKind::NonValue("enum"))
                            | Some(GlobalKind::Table)
                    );
                if !is_type_param && !is_known {
                    self.err(name.span, format!("unknown type name `{}`", name.node), None);
                }
                for a in args {
                    self.check_type(a);
                }
            }
            TypeKind::Key(t) | TypeKind::Value(t) => {
                if !matches!(self.globals.get(&t.node), Some(GlobalKind::Table)) {
                    self.err(t.span, format!("unknown table `{}` in table-derived type", t.node), None);
                }
            }
            TypeKind::Option(inner) | TypeKind::Vector(inner) | TypeKind::Set(inner)
            | TypeKind::Bag(inner) => self.check_type(inner),
            TypeKind::Map(k, v) | TypeKind::Table(k, v) | TypeKind::Fun(k, v) => {
                self.check_type(k);
                self.check_type(v);
            }
            TypeKind::Tuple(items) => {
                for t in items {
                    self.check_type(t);
                }
            }
            TypeKind::Record(fields) => {
                for (_, t) in fields {
                    self.check_type(t);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::{decl, expr, pat, ty};

    fn resolve(items: Vec<Item>) -> Result<ResolvedModule, DiagBag> {
        resolve_module(decl::module("test", items), &[])
    }

    fn msgs(bag: &DiagBag) -> Vec<String> {
        bag.errors().iter().map(|e| e.message().to_string()).collect()
    }

    fn users_table() -> Item {
        decl::table("users", vec![("id", ty::int()), ("name", ty::string())], &["id"])
    }

    // ---- scoping ----------------------------------------------------------------

    #[test]
    fn shadowing_local_over_global() {
        // const x; function f() == { let x == 1; x } — inner x shadows const.
        let items = vec![
            decl::const_("x", ty::int(), expr::int(0)),
            decl::function(
                "f",
                vec![],
                ty::int(),
                expr::block(vec![expr::let_(pat::bind("x"), expr::int(1))], expr::var("x")),
            ),
        ];
        let r = resolve(items).expect("shadowing is legal");
        // The tail `x` must resolve to the local binding.
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        let ExprKind::Block { tail, .. } = &op.body.as_ref().unwrap().kind else { panic!() };
        let ExprKind::Var(v) = &tail.kind else { panic!() };
        assert_eq!(r.resolved.vars.get(&v.span), Some(&VarRes::Local));
    }

    #[test]
    fn undefined_name_errors() {
        let items = vec![decl::function("f", vec![], ty::int(), expr::var("nope"))];
        let bag = resolve(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("undefined name `nope`")));
    }

    #[test]
    fn duplicate_module_level_name_errors() {
        let items = vec![
            decl::function("f", vec![], ty::int(), expr::int(1)),
            decl::function("f", vec![], ty::int(), expr::int(2)),
        ];
        let bag = resolve(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("duplicate module-level name `f`")));
    }

    #[test]
    fn reserved_primitive_name_as_binding_errors() {
        let items = vec![decl::function(
            "f",
            vec![],
            ty::int(),
            expr::block(vec![expr::let_(pat::bind("read"), expr::int(1))], expr::var("read")),
        )];
        let bag = resolve(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("reserved effect-primitive")));
    }

    // ---- captures (§4.2) ---------------------------------------------------------

    #[test]
    fn missing_capture_errors() {
        // { let x == 1; lambda(y) { x + y } } — x used but not captured.
        let body = expr::block(
            vec![expr::let_(pat::bind("x"), expr::int(1))],
            expr::lambda(
                &[],
                vec![pat::bind("y")],
                expr::binop(BinOpKind::Add, expr::var("x"), expr::var("y")),
            ),
        );
        let bag = resolve(vec![decl::function("f", vec![], ty::int(), body)]).unwrap_err();
        assert!(msgs(&bag)
            .iter()
            .any(|m| m.contains("outer binding `x` is used but not listed")));
    }

    #[test]
    fn unused_capture_warns() {
        let items = vec![decl::function(
            "f",
            vec![],
            ty::int(),
            expr::block(
                vec![expr::let_(pat::bind("x"), expr::int(1))],
                expr::lambda(&["x"], vec![pat::bind("y")], expr::var("y")),
            ),
        )];
        // Warnings don't fail the pass; inspect the resolver's bag directly.
        let mut rr = Resolver::new(NamedSource::new("t.cql", String::new()));
        let mut m = decl::module("t", items);
        rr.collect_globals(&m, &[]);
        rr.resolve_items(&mut m.items);
        assert!(!rr.diags.has_errors());
        assert!(rr
            .diags
            .warnings()
            .iter()
            .any(|w| w.message().contains("capture `x` is declared but never used")));
    }

    #[test]
    fn nested_lambda_capture_propagates() {
        // { let x == 1; lambda [x](y) { lambda [x](z) { x } } }
        // Inner lambda captures x from the outer lambda; the outer must also
        // capture it from the let. If the outer omits it, we get an error.
        let inner = expr::lambda(&["x"], vec![pat::bind("z")], expr::var("x"));
        let outer_missing = expr::lambda(&[], vec![pat::bind("y")], inner.clone());
        let body = expr::block(vec![expr::let_(pat::bind("x"), expr::int(1))], outer_missing);
        let bag = resolve(vec![decl::function("f", vec![], ty::int(), body)]).unwrap_err();
        assert!(msgs(&bag)
            .iter()
            .any(|m| m.contains("outer binding `x` is used but not listed")));

        // Fully declared: ok.
        let inner = expr::lambda(&["x"], vec![pat::bind("z")], expr::var("x"));
        let outer_ok = expr::lambda(&["x"], vec![pat::bind("y")], inner);
        let body = expr::block(vec![expr::let_(pat::bind("x"), expr::int(1))], outer_ok);
        resolve(vec![decl::function("f", vec![], ty::int(), body)]).expect("nested captures ok");
    }

    #[test]
    fn top_level_names_are_not_captures() {
        // lambda body referencing a function/const/stdlib name needs no capture.
        let body = expr::lambda(
            &[],
            vec![pat::bind("y")],
            expr::call("g", vec![expr::var("y"), expr::var("C")]),
        );
        let items = vec![
            decl::const_("C", ty::int(), expr::int(3)),
            decl::function(
                "g",
                vec![decl::param("a", ty::int()), decl::param("b", ty::int())],
                ty::int(),
                expr::call("max", vec![expr::var("a"), expr::var("b")]),
            ),
            decl::function("f", vec![], ty::int(), body),
        ];
        let r = resolve(items).expect("top-level refs are free in lambdas");
        let Item::Operator(op) = &r.module.items[2] else { panic!() };
        let ExprKind::Lambda(l) = &op.body.as_ref().unwrap().kind else { panic!() };
        let ExprKind::Call(c) = &l.body.kind else { panic!() };
        assert!(matches!(
            r.resolved.callee.get(&c.name.span),
            Some(Callee::Operator { name, .. }) if name == "g"
        ));
    }

    // ---- table positions (§4.3) -----------------------------------------------------

    #[test]
    fn read_rewrites_to_read_prim() {
        let body = expr::call(
            "read",
            vec![expr::var("users"), expr::lambda(&[], vec![pat::bind("u")], expr::bool_(true))],
        );
        let items = vec![users_table(), decl::query("q", vec![], ty::int(), body)];
        let r = resolve(items).expect("read in query");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        let ExprKind::ReadPrim { table, .. } = &op.body.as_ref().unwrap().kind else {
            panic!("expected ReadPrim, got {:?}", op.body)
        };
        assert_eq!(table.node, "users");
    }

    #[test]
    fn lookup_kept_as_call_and_marked() {
        let call = expr::call("lookup", vec![expr::var("users"), expr::int(1)]);
        let span = match &call.kind {
            ExprKind::Call(c) => c.name.span,
            _ => unreachable!(),
        };
        let items = vec![users_table(), decl::query("q", vec![], ty::int(), call)];
        let r = resolve(items).expect("lookup in query");
        assert_eq!(r.resolved.callee.get(&span), Some(&Callee::LookupPrim));
    }

    #[test]
    fn table_name_outside_table_position_errors() {
        let items = vec![
            users_table(),
            decl::query("q", vec![], ty::int(), expr::var("users")),
        ];
        let bag = resolve(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("only allowed in table position")));
    }

    #[test]
    fn table_name_sugar_in_generator_source_ok() {
        let body = expr::set_map(expr::var("u"), vec![expr::gen(pat::bind("u"), expr::var("users"))]);
        let items = vec![users_table(), decl::query("q", vec![], ty::int(), body)];
        let r = resolve(items).expect("table sugar in generator");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        let ExprKind::SetMap { gens, .. } = &op.body.as_ref().unwrap().kind else { panic!() };
        let ExprKind::Var(v) = &gens[0].source.kind else { panic!() };
        assert_eq!(r.resolved.vars.get(&v.span), Some(&VarRes::TableSugar));
    }

    #[test]
    fn write_con_rewrite_and_table_check() {
        let body = expr::call(
            "insert",
            vec![expr::var("users"), expr::record_lit(vec![("id", expr::int(1))])],
        );
        let items = vec![users_table(), decl::action("a", vec![], body)];
        let r = resolve(items).expect("insert in action");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        assert!(matches!(op.body.as_ref().unwrap().kind, ExprKind::WriteCon(WriteCon::Insert { .. })));

        // Unknown table in primitive position.
        let bad = expr::call("delete", vec![expr::var("nope"), expr::int(1)]);
        let items = vec![decl::action("a", vec![], bad)];
        let bag = resolve(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("must be a table declared in this module")));
    }

    // ---- enum variants ----------------------------------------------------------------

    #[test]
    fn enum_variant_call_rewrites() {
        let items = vec![
            decl::enum_("shape", vec![decl::variant_tuple("circle", vec![ty::float()])]),
            decl::function("f", vec![], ty::named("shape"), expr::call("circle", vec![expr::float(1.0)])),
        ];
        let r = resolve(items).expect("variant construction");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        match &op.body.as_ref().unwrap().kind {
            ExprKind::EnumConstruct { name, args } => {
                assert_eq!(name.node, "circle");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected EnumConstruct, got {other:?}"),
        }
    }

    #[test]
    fn unit_variant_var_rewrites() {
        let items = vec![
            decl::enum_("color", vec![decl::variant_unit("red")]),
            decl::function("f", vec![], ty::named("color"), expr::var("red")),
        ];
        let r = resolve(items).expect("unit variant as value");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        assert!(matches!(op.body.as_ref().unwrap().kind, ExprKind::EnumConstruct { .. }));
    }

    // ---- named arguments (A.3) ----------------------------------------------------------

    fn binop_f() -> Item {
        decl::function(
            "f",
            vec![decl::param("a", ty::int()), decl::param("b", ty::int())],
            ty::int(),
            expr::var("a"),
        )
    }

    #[test]
    fn named_args_ok_and_errors() {
        // ok: named after positional
        let ok = expr::call_args("f", vec![expr::arg(expr::int(1)), expr::named_arg("b", expr::int(2))]);
        resolve(vec![binop_f(), decl::function("g", vec![], ty::int(), ok)]).expect("named args ok");

        // error: positional after named
        let bad_order = expr::call_args(
            "f",
            vec![expr::named_arg("a", expr::int(1)), expr::arg(expr::int(2))],
        );
        let bag = resolve(vec![binop_f(), decl::function("g", vec![], ty::int(), bad_order)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("positional argument follows a named argument")));

        // error: duplicate
        let dup = expr::call_args(
            "f",
            vec![expr::arg(expr::int(1)), expr::named_arg("a", expr::int(2))],
        );
        let bag = resolve(vec![binop_f(), decl::function("g", vec![], ty::int(), dup)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("passed more than once")));

        // error: missing
        let missing = expr::call_args("f", vec![expr::named_arg("a", expr::int(1))]);
        let bag = resolve(vec![binop_f(), decl::function("g", vec![], ty::int(), missing)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("missing argument")));

        // error: unknown name
        let unknown = expr::call_args(
            "f",
            vec![expr::named_arg("a", expr::int(1)), expr::named_arg("zz", expr::int(2))],
        );
        let bag = resolve(vec![binop_f(), decl::function("g", vec![], ty::int(), unknown)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("unknown argument name `zz`")));

        // error: named arg to stdlib also validated
        let std_bad = expr::call_args(
            "max",
            vec![expr::named_arg("nope", expr::int(1)), expr::named_arg("b", expr::int(2))],
        );
        let bag = resolve(vec![decl::function("g", vec![], ty::int(), std_bad)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("unknown argument name `nope`")));

        // error: named arg to a primitive
        let prim = expr::call_args(
            "read",
            vec![
                expr::named_arg("t", expr::var("users")),
                expr::named_arg("p", expr::bool_(true)),
            ],
        );
        let bag = resolve(vec![users_table(), decl::query("q", vec![], ty::int(), prim)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("named arguments are not allowed")));
    }

    // ---- stdlib shadowing ----------------------------------------------------------------

    #[test]
    fn user_binding_shadows_stdlib() {
        // A local `max` shadows the stdlib function (A.3).
        let body = expr::block(
            vec![expr::let_(
                pat::bind("max"),
                expr::lambda(&[], vec![pat::bind("x")], expr::var("x")),
            )],
            expr::call("max", vec![expr::int(1)]),
        );
        let r = resolve(vec![decl::function("f", vec![], ty::int(), body)]).expect("shadowing stdlib ok");
        let Item::Operator(op) = &r.module.items[0] else { panic!() };
        let ExprKind::Block { tail, .. } = &op.body.as_ref().unwrap().kind else { panic!() };
        let ExprKind::Call(c) = &tail.kind else { panic!() };
        assert_eq!(r.resolved.callee.get(&c.name.span), Some(&Callee::LocalValue));

        // A module-level function named `max` also shadows the stdlib.
        let items = vec![
            decl::function("max", vec![decl::param("x", ty::int())], ty::int(), expr::var("x")),
            decl::function("g", vec![], ty::int(), expr::call("max", vec![expr::int(1)])),
        ];
        let r = resolve(items).expect("module-level stdlib shadowing ok");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        let ExprKind::Call(c) = &op.body.as_ref().unwrap().kind else { panic!() };
        assert!(matches!(
            r.resolved.callee.get(&c.name.span),
            Some(Callee::Operator { name, module_local: true, .. }) if name == "max"
        ));
    }

    // ---- imports -------------------------------------------------------------------------

    fn common_import() -> ImportedModule {
        ImportedModule {
            path: vec!["common".into(), "math".into()],
            public_items: vec![
                ImportedItem {
                    name: "double".into(),
                    kind: ImportedKind::Function,
                    params: Some(vec!["x".into()]),
                },
                ImportedItem {
                    name: "point".into(),
                    kind: ImportedKind::Enum,
                    params: None,
                },
                ImportedItem {
                    name: "origin".into(),
                    kind: ImportedKind::EnumVariant { arity: 0 },
                    params: None,
                },
            ],
        }
    }

    #[test]
    fn use_imports_public_items() {
        let body = expr::call("double", vec![expr::int(2)]);
        let items = vec![
            Item::Use(UseDecl { path: vec![crate::ast::builder::id("common"), crate::ast::builder::id("math")], alias: None }),
            decl::function("f", vec![], ty::int(), body),
        ];
        let r = resolve_module(decl::module("m", items), &[common_import()]).expect("import works");
        let Item::Operator(op) = &r.module.items[1] else { panic!() };
        let ExprKind::Call(c) = &op.body.as_ref().unwrap().kind else { panic!() };
        assert!(matches!(
            r.resolved.callee.get(&c.name.span),
            Some(Callee::Operator { name, module_local: false, .. }) if name == "double"
        ));
        // Imported enum is known for inductive-type recognition.
        assert!(r.resolved.known_enums.contains("point"));
    }

    #[test]
    fn use_alias_warns_and_conflict_errors() {
        let u = Item::Use(UseDecl {
            path: vec![crate::ast::builder::id("common"), crate::ast::builder::id("math")],
            alias: Some(crate::ast::builder::id("m")),
        });
        // alias warning
        let items = vec![u, decl::function("f", vec![], ty::int(), expr::int(1))];
        let mut rr = Resolver::new(NamedSource::new("t.cql", String::new()));
        let mut m = decl::module("t", items);
        rr.collect_globals(&m, &[common_import()]);
        rr.resolve_items(&mut m.items);
        assert!(rr.diags.warnings().iter().any(|w| w.message().contains("module alias `m`")));

        // conflict with module-level name
        let u2 = Item::Use(UseDecl {
            path: vec![crate::ast::builder::id("common"), crate::ast::builder::id("math")],
            alias: None,
        });
        let items = vec![
            u2,
            decl::function("double", vec![decl::param("x", ty::int())], ty::int(), expr::var("x")),
        ];
        let bag = resolve_module(decl::module("t", items), &[common_import()]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("conflicts with an existing module-level name")));

        // unresolved import
        let u3 = Item::Use(UseDecl {
            path: vec![crate::ast::builder::id("nope")],
            alias: None,
        });
        let bag = resolve_module(decl::module("t", vec![u3]), &[common_import()]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("unresolved import")));
    }
}
