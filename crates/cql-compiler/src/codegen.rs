//! Code generation backends (doc/codegen-backend.md §4).
//!
//! Backends consume [`CirModule`] only — never the AST. The MVP backend is
//! [`RustBackend`], rendering CIR to a single Rust source file that depends
//! on `cql-runtime`, via an askama template (`templates/module.rs`) for the
//! module skeleton plus a recursive emitter for declarations.
//!
//! # ABI decisions (cql.md §6.2 mapping to cql-runtime)
//!
//! | CQL type | generated Rust |
//! | --- | --- |
//! | `bool`/`int`/`float`/`string` | `bool`/`i64`/`f64`/`String` |
//! | `date` / `decimal(m,n)` | `cql_runtime::Date` / `cql_runtime::Decimal` |
//! | `option`/`vector`/`set`/`bag`/`map` | `Option`/`Vec`/`CqlSet`/`CqlBag`/`CqlMap` |
//! | tuple | Rust tuple |
//! | record (structural) | interned `Rec_<hash>` struct (fields sorted by name) |
//! | table row / key | `<Table>Row` / `<Table>Key` structs |
//! | enum | same-named Rust enum (self-recursive payloads boxed) |
//! | `T -> U` | `Rc<dyn Fn(T) -> U>` (lambda lifting + `MakeClosure`) |
//! | `write_op` | `cql_runtime::WriteOp` (type-erased, §3.6) |
//!
//! §6.2 specifies `date`/`decimal` as ABI *records*; the Rust backend keeps
//! the runtime's native `Date`/`Decimal` newtypes instead (the ABI mapping
//! matters only at the WASM component boundary).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use askama::Template;

use crate::ast::{BinOpKind, EffectLevel, PatLit, UnOpKind};
use crate::cir::*;
use crate::diag::DiagBag;
use crate::optimize::ReadPlan;

/// A code generation backend (doc/codegen-backend.md §4).
pub trait Backend {
    /// What the backend produces (Rust: one source file's text).
    type Output;
    /// Stable backend name (e.g. `"rust"`, `"mududb"`).
    fn name(&self) -> &'static str;
    /// Emit the backend output for a lowered CIR module.
    fn emit(&self, cir: &CirModule, ctx: &EmitCtx) -> Result<Self::Output, DiagBag>;
}

/// Backend-independent emission options.
#[derive(Debug, Clone)]
pub struct EmitCtx {
    /// Name of the module being emitted (for the file header).
    pub module_name: String,
    /// Emit CQL `test` blocks as Rust `#[test]` functions.
    pub emit_tests: bool,
}

impl EmitCtx {
    /// Create a context for `module_name` with test emission enabled.
    pub fn new(module_name: impl Into<String>) -> Self {
        EmitCtx {
            module_name: module_name.into(),
            emit_tests: true,
        }
    }
}

impl Default for EmitCtx {
    fn default() -> Self {
        EmitCtx::new("module")
    }
}

/// The MVP backend: CIR → Rust source depending on `cql-runtime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustBackend;

impl Backend for RustBackend {
    type Output = String;

    fn name(&self) -> &'static str {
        "rust"
    }

    fn emit(&self, cir: &CirModule, ctx: &EmitCtx) -> Result<String, DiagBag> {
        let mut e = Emitter::new(cir);
        let mut records = e.emit_records();
        let enums = e.emit_enums();
        let state = e.emit_state();
        let functions = e.emit_functions();
        let tests = if ctx.emit_tests { e.emit_tests() } else { Vec::new() };
        if e.needs_f64.get() {
            records.insert(0, CQL_F64_DEF.to_string());
        }
        let t = ModuleTemplate {
            module_name: ctx.module_name.clone(),
            records,
            enums,
            state,
            functions,
            tests,
        };
        let out = t.render().expect("askama template rendering is infallible");
        let Emitter { bag, .. } = e;
        bag.into_result(out)
    }
}

/// Askama render model: every section is pre-rendered by the emitter; the
/// template supplies the module skeleton (header, imports, helpers).
#[derive(Template)]
#[template(path = "module.txt")]
struct ModuleTemplate {
    module_name: String,
    records: Vec<String>,
    enums: Vec<String>,
    state: String,
    functions: Vec<String>,
    tests: Vec<String>,
}

// ---------------------------------------------------------------------------
// The emitter
// ---------------------------------------------------------------------------

struct Emitter<'a> {
    cir: &'a CirModule,
    bag: DiagBag,
    /// Fresh-name counter for pattern-guard bindings.
    tmp: usize,
    /// Lifted functions whose body (transitively) needs the table state.
    fn_state: HashMap<String, bool>,
    /// Whether any emitted type mentions a collection of floats (needs the
    /// `CqlF64` wrapper, since `f64: Eq + Hash` does not hold).
    needs_f64: Cell<bool>,
}

impl<'a> Emitter<'a> {
    fn new(cir: &'a CirModule) -> Self {
        Emitter {
            cir,
            bag: DiagBag::new(),
            tmp: 0,
            fn_state: compute_fn_state(cir),
            needs_f64: Cell::new(false),
        }
    }

    fn table(&self, name: &str) -> &CirTable {
        self.cir
            .tables
            .iter()
            .find(|t| t.name == name)
            .expect("CIR table reference resolves")
    }

    fn table_by_row(&self, row: &str) -> &CirTable {
        self.cir
            .tables
            .iter()
            .find(|t| t.row == row)
            .expect("CIR row reference resolves")
    }

    fn record(&self, name: &str) -> &CirRecordDef {
        self.cir
            .records
            .iter()
            .find(|r| r.name == name)
            .expect("CIR record reference resolves")
    }

    fn enum_def(&self, name: &str) -> &CirEnumDef {
        self.cir
            .enums
            .iter()
            .find(|e| e.name == name)
            .expect("CIR enum reference resolves")
    }

    fn variant(&self, enum_name: &str, variant: &str) -> &CirVariant {
        self.enum_def(enum_name)
            .variants
            .iter()
            .find(|v| v.name == variant)
            .expect("CIR variant reference resolves")
    }

    // -- types ----------------------------------------------------------------

    /// Rust type for a CIR type. Collection element positions go through
    /// [`Emitter::coll_ty_str`] so float elements get the `CqlF64` wrapper
    /// (`f64` lacks `Eq`/`Hash`, which `CqlSet`/`CqlBag`/`CqlMap` require).
    fn ty_str(&self, ty: &CirType) -> String {
        match ty {
            CirType::Bool => "bool".into(),
            CirType::Int => "i64".into(),
            CirType::Float => "f64".into(),
            CirType::Decimal(_) => "Decimal".into(),
            CirType::String => "String".into(),
            CirType::Date => "Date".into(),
            CirType::Option(t) => format!("Option<{}>", self.ty_str(t)),
            CirType::Vector(t) => format!("Vec<{}>", self.ty_str(t)),
            CirType::Set(t) => format!("CqlSet<{}>", self.coll_ty_str(t)),
            CirType::Bag(t) => format!("CqlBag<{}>", self.coll_ty_str(t)),
            CirType::Map(k, v) => {
                format!("CqlMap<{}, {}>", self.coll_ty_str(k), self.coll_ty_str(v))
            }
            CirType::Tuple(ts) => {
                format!("({})", ts.iter().map(|t| self.ty_str(t)).collect::<Vec<_>>().join(", "))
            }
            CirType::Record(n) | CirType::Row(n) | CirType::Enum(n) => n.clone(),
            CirType::Fun(a, b) => {
                format!("Rc<dyn Fn({}) -> {}>", self.ty_str(a), self.ty_str(b))
            }
            CirType::WriteOp => "WriteOp".into(),
        }
    }

    /// Type used inside collections: floats (and composites containing
    /// them, except records/enums which have manual trait impls) are wrapped
    /// in `CqlF64`.
    fn coll_ty_str(&self, ty: &CirType) -> String {
        match ty {
            CirType::Float => {
                self.needs_f64.set(true);
                "CqlF64".into()
            }
            CirType::Option(t) => format!("Option<{}>", self.coll_ty_str(t)),
            CirType::Vector(t) => format!("Vec<{}>", self.coll_ty_str(t)),
            CirType::Tuple(ts) => format!(
                "({})",
                ts.iter().map(|t| self.coll_ty_str(t)).collect::<Vec<_>>().join(", ")
            ),
            CirType::Set(t) => format!("CqlSet<{}>", self.coll_ty_str(t)),
            CirType::Bag(t) => format!("CqlBag<{}>", self.coll_ty_str(t)),
            CirType::Map(k, v) => {
                format!("CqlMap<{}, {}>", self.coll_ty_str(k), self.coll_ty_str(v))
            }
            other => self.ty_str(other),
        }
    }
}

/// Whether a type contains a float outside of record/enum payloads (those
/// have manual `Eq`/`Hash`/`CanonOrd` impls and stay as-is).
fn needs_wrap(ty: &CirType) -> bool {
    match ty {
        CirType::Float => true,
        CirType::Option(t) | CirType::Vector(t) | CirType::Set(t) | CirType::Bag(t) => {
            needs_wrap(t)
        }
        CirType::Map(k, v) => needs_wrap(k) || needs_wrap(v),
        CirType::Tuple(ts) => ts.iter().any(needs_wrap),
        _ => false,
    }
}

/// Which lifted functions need the table state: bodies containing reads or
/// L1/L2 calls, transitively through `MakeClosure` edges.
fn compute_fn_state(cir: &CirModule) -> HashMap<String, bool> {
    fn direct(e: &CirExpr) -> (bool, Vec<String>) {
        // (directly needs state, MakeClosure targets)
        let mut need = false;
        let mut targets = Vec::new();
        fn walk(e: &CirExpr, need: &mut bool, targets: &mut Vec<String>) {
            match &e.kind {
                CirExprKind::Read { .. } => *need = true,
                CirExprKind::Call {
                    callee: CirCallee::Operator { level, .. },
                    ..
                } if *level != EffectLevel::Function => *need = true,
                CirExprKind::MakeClosure { fun, env } => {
                    targets.push(fun.clone());
                    for (_, x) in env {
                        walk(x, need, targets);
                    }
                    return;
                }
                _ => {}
            }
            cir_children(e, &mut |c| walk(c, need, targets));
        }
        walk(e, &mut need, &mut targets);
        (need, targets)
    }
    let mut state: HashMap<String, bool> = HashMap::new();
    let mut edges: Vec<(String, Vec<String>)> = Vec::new();
    for f in &cir.functions {
        if f.env.is_none() {
            continue; // plain L0 functions are pure by the effect discipline
        }
        let (need, targets) = direct(&f.body);
        state.insert(f.name.clone(), need);
        edges.push((f.name.clone(), targets));
    }
    loop {
        let mut changed = false;
        for (f, targets) in &edges {
            if state.get(f).copied().unwrap_or(false) {
                continue;
            }
            if targets.iter().any(|t| state.get(t).copied().unwrap_or(false)) {
                state.insert(f.clone(), true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    state
}

/// Child expressions of a CIR node (for codegen-local walks).
fn cir_children<'e>(e: &'e CirExpr, f: &mut impl FnMut(&'e CirExpr)) {
    match &e.kind {
        CirExprKind::Deref(x) => f(x),
        CirExprKind::MakeClosure { env, .. } => {
            for (_, x) in env {
                f(x);
            }
        }
        CirExprKind::Call { args, .. } => {
            for a in args {
                f(a);
            }
        }
        CirExprKind::App { func, args } => {
            f(func);
            for a in args {
                f(a);
            }
        }
        CirExprKind::Let { value, body, .. } => {
            f(value);
            f(body);
        }
        CirExprKind::If {
            cond,
            then_br,
            else_br,
        } => {
            f(cond);
            f(then_br);
            f(else_br);
        }
        CirExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for a in arms {
                f(&a.body);
            }
        }
        CirExprKind::RecordLit { fields, .. } | CirExprKind::RecordUpd { fields, .. } => {
            for (_, x) in fields {
                f(x);
            }
            if let CirExprKind::RecordUpd { base, .. } = &e.kind {
                f(base);
            }
        }
        CirExprKind::Tuple(xs) | CirExprKind::Vector(xs) | CirExprKind::Set(xs)
        | CirExprKind::Bag(xs) => {
            for x in xs {
                f(x);
            }
        }
        CirExprKind::MapLit(kvs) => {
            for (k, v) in kvs {
                f(k);
                f(v);
            }
        }
        CirExprKind::OptionSome(x) => f(x),
        CirExprKind::BinOp { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        CirExprKind::UnOp { operand, .. } => f(operand),
        CirExprKind::Field { base, .. } | CirExprKind::TupleProj { base, .. } => f(base),
        CirExprKind::EnumConstruct { args, .. } => {
            for a in args {
                f(a);
            }
        }
        CirExprKind::Cast { expr, .. } => f(expr),
        CirExprKind::Read { key, predicate, .. } => {
            for (_, x) in key {
                f(x);
            }
            f(predicate);
        }
        CirExprKind::WriteOp(w) => match w {
            CirWriteOp::Insert { row, .. } => f(row),
            CirWriteOp::Update {
                key, transform, ..
            } => {
                f(key);
                f(transform);
            }
            CirWriteOp::Delete { key, .. } => f(key),
        },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Declaration emission
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    fn err(&mut self, msg: impl Into<String>) {
        self.bag.push_error(crate::diag::CqlError::new(
            miette::NamedSource::new("codegen", String::new()),
            crate::ast::Span { start: 0, end: 0 },
            msg,
            None,
        ));
    }

    /// Function values cannot appear in records/enums/rows: the generated
    /// structs derive `PartialEq`/`Hash`, which `Rc<dyn Fn>` lacks.
    fn check_no_fun_fields(&mut self, what: &str, fields: &[(String, CirType)]) {
        for (n, t) in fields {
            if contains_fun_or_writeop(t) {
                self.err(format!(
                    "field `{n}` of {what} has a function/write_op type; \
                     such values cannot be stored in records, enums or table rows"
                ));
            }
        }
    }

    fn emit_records(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.cir.records {
            self.check_no_fun_fields(&format!("record `{}`", r.name), &r.fields);
            out.push(self.record_struct(&r.name, &r.fields));
        }
        out
    }

    /// A plain data struct with the runtime trait impls CQL values need:
    /// `Eq` + `Hash` + `CanonOrd` (all in canonical order).
    fn record_struct(&self, name: &str, fields: &[(String, CirType)]) -> String {
        let mut s = String::new();
        s.push_str("#[derive(Debug, Clone, PartialEq)]\n");
        s.push_str(&format!("pub struct {name} {{\n"));
        for (f, t) in fields {
            s.push_str(&format!("    pub {}: {},\n", sanitize(f), self.ty_str(t)));
        }
        s.push_str("}\n");
        s.push_str(&format!("impl Eq for {name} {{}}\n"));
        s.push_str(&format!("impl Hash for {name} {{\n"));
        s.push_str("    fn hash<H: Hasher>(&self, state: &mut H) {\n");
        for (f, t) in fields {
            self.hash_field(&mut s, &format!("&self.{}", sanitize(f)), t);
        }
        s.push_str("    }\n}\n");
        s.push_str(&format!("impl CanonOrd for {name} {{\n"));
        s.push_str("    fn canon_cmp(&self, other: &Self) -> Ordering {\n");
        s.push_str(&self.canon_chain(fields, "self", "other"));
        s.push_str("    }\n}\n");
        s
    }

    /// `a.f1.cmp(b.f1).then_with(|| a.f2.cmp(b.f2))...` for record fields.
    fn canon_chain(&self, fields: &[(String, CirType)], a: &str, b: &str) -> String {
        if fields.is_empty() {
            return "        Ordering::Equal\n".to_string();
        }
        let mut it = fields.iter();
        let (f0, _) = it.next().unwrap();
        let mut s = format!(
            "        ({a}.{}).canon_cmp(&({b}.{}))",
            sanitize(f0),
            sanitize(f0)
        );
        for (f, _) in it {
            s.push_str(&format!(
                ".then_with(|| ({a}.{}).canon_cmp(&({b}.{})))",
                sanitize(f),
                sanitize(f)
            ));
        }
        s.push('\n');
        s
    }

    /// Hash one value, given a `&T` expression. Recurses through composite
    /// types so `f64` members hash by bits.
    fn hash_field(&self, out: &mut String, code: &str, ty: &CirType) {
        match ty {
            CirType::Float => out.push_str(&format!("        (*{code}).to_bits().hash(state);\n")),
            CirType::Bool
            | CirType::Int
            | CirType::String
            | CirType::Date
            | CirType::Decimal(_)
            | CirType::Record(_)
            | CirType::Row(_)
            | CirType::Enum(_) => out.push_str(&format!("        ({code}).hash(state);\n")),
            CirType::Option(t) => {
                out.push_str(&format!("        match {code} {{\n"));
                out.push_str("            Some(__h) => { 1u8.hash(state);\n");
                self.hash_field(out, "__h", t);
                out.push_str("            }\n            None => 0u8.hash(state),\n        }\n");
            }
            CirType::Tuple(ts) => {
                for (i, t) in ts.iter().enumerate() {
                    self.hash_field(out, &format!("&({code}).{i}"), t);
                }
            }
            CirType::Vector(t) => {
                out.push_str(&format!("        ({code}).len().hash(state);\n"));
                out.push_str(&format!("        for __h in ({code}).iter() {{\n"));
                self.hash_field(out, "__h", t);
                out.push_str("        }\n");
            }
            CirType::Set(t) => {
                out.push_str(&format!("        ({code}).len().hash(state);\n"));
                out.push_str(&format!("        for __h in ({code}).iter() {{\n"));
                self.hash_coll_elem(out, "__h", t);
                out.push_str("        }\n");
            }
            CirType::Bag(t) => {
                out.push_str(&format!("        ({code}).total_count().hash(state);\n"));
                out.push_str(&format!("        for (__h, __c) in ({code}).entries() {{\n"));
                self.hash_coll_elem(out, "__h", t);
                out.push_str("            __c.hash(state);\n        }\n");
            }
            CirType::Map(k, v) => {
                out.push_str(&format!("        ({code}).len().hash(state);\n"));
                out.push_str(&format!("        for (__hk, __hv) in ({code}).iter() {{\n"));
                self.hash_coll_elem(out, "__hk", k);
                self.hash_coll_elem(out, "__hv", v);
                out.push_str("        }\n");
            }
            // Rejected earlier by check_no_fun_fields; emit nothing.
            CirType::Fun(..) | CirType::WriteOp => {}
        }
    }

    fn emit_enums(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.cir.enums {
            for v in &e.variants {
                let tys: Vec<(String, CirType)> = match &v.payload {
                    CirVariantPayload::None => vec![],
                    CirVariantPayload::Tuple(ts, _) => {
                        ts.iter().enumerate().map(|(i, t)| (i.to_string(), t.clone())).collect()
                    }
                    CirVariantPayload::Record(r) => {
                        vec![("0".into(), CirType::Record(r.clone()))]
                    }
                };
                self.check_no_fun_fields(&format!("enum `{}`", e.name), &tys);
            }
            out.push(self.enum_decl(e));
        }
        out
    }

    fn enum_decl(&self, e: &CirEnumDef) -> String {
        let mut s = String::new();
        s.push_str("#[derive(Debug, Clone, PartialEq)]\n");
        s.push_str(&format!("pub enum {} {{\n", e.name));
        for v in &e.variants {
            match &v.payload {
                CirVariantPayload::None => s.push_str(&format!("    {},\n", v.name)),
                CirVariantPayload::Tuple(ts, boxed) => {
                    let args: Vec<String> = ts
                        .iter()
                        .zip(boxed)
                        .map(|(t, b)| {
                            if *b {
                                format!("Box<{}>", self.ty_str(t))
                            } else {
                                self.ty_str(t)
                            }
                        })
                        .collect();
                    s.push_str(&format!("    {}({}),\n", v.name, args.join(", ")));
                }
                CirVariantPayload::Record(r) => {
                    s.push_str(&format!("    {}({}),\n", v.name, r));
                }
            }
        }
        s.push_str("}\n");
        s.push_str(&format!("impl Eq for {} {{}}\n", e.name));
        // Hash: variant rank, then payloads.
        s.push_str(&format!("impl Hash for {} {{\n", e.name));
        s.push_str("    fn hash<H: Hasher>(&self, state: &mut H) {\n");
        s.push_str("        std::mem::discriminant(self).hash(state);\n");
        s.push_str("        match self {\n");
        for v in &e.variants {
            let tys = self.variant_tys(v);
            if tys.is_empty() {
                s.push_str(&format!("            {}::{} => {{}}\n", e.name, v.name));
                continue;
            }
            let binds: Vec<String> = (0..tys.len()).map(|i| format!("__v{i}")).collect();
            s.push_str(&format!(
                "            {}::{}({}) => {{\n",
                e.name,
                v.name,
                binds.join(", ")
            ));
            for (i, (t, b)) in tys.iter().enumerate() {
                if *b {
                    self.hash_field(&mut s, &format!("&**__v{i}"), t);
                } else {
                    self.hash_field(&mut s, &format!("__v{i}"), t);
                }
            }
            s.push_str("            }\n");
        }
        s.push_str("        }\n    }\n}\n");
        // CanonOrd: variant rank, then payloads in order.
        s.push_str(&format!("impl CanonOrd for {} {{\n", e.name));
        s.push_str("    fn canon_cmp(&self, other: &Self) -> Ordering {\n");
        s.push_str("        let __rank = |x: &Self| -> u32 { match x {\n");
        for (i, v) in e.variants.iter().enumerate() {
            let dots = if self.variant_tys(v).is_empty() { "" } else { "(..)" };
            s.push_str(&format!("            {}::{}{} => {},\n", e.name, v.name, dots, i));
        }
        s.push_str("        } };\n");
        s.push_str("        __rank(self).cmp(&__rank(other)).then_with(|| match (self, other) {\n");
        for v in &e.variants {
            let tys = self.variant_tys(v);
            if tys.is_empty() {
                s.push_str(&format!(
                    "            ({}::{}, {}::{}) => Ordering::Equal,\n",
                    e.name, v.name, e.name, v.name
                ));
                continue;
            }
            let lhs: Vec<String> = (0..tys.len()).map(|i| format!("__a{i}")).collect();
            let rhs: Vec<String> = (0..tys.len()).map(|i| format!("__b{i}")).collect();
            s.push_str(&format!(
                "            ({}::{}({}), {}::{}({})) => {{\n",
                e.name,
                v.name,
                lhs.join(", "),
                e.name,
                v.name,
                rhs.join(", ")
            ));
            let mut chain: Option<String> = None;
            for (i, (_t, b)) in tys.iter().enumerate() {
                let cmp = if *b {
                    format!("(**__a{i}).canon_cmp(&(**__b{i}))")
                } else {
                    format!("(__a{i}).canon_cmp(&(__b{i}))")
                };
                chain = Some(match chain {
                    None => format!("                {cmp}"),
                    Some(c) => format!("{c}.then_with(|| {cmp})"),
                });
            }
            s.push_str(&chain.unwrap());
            s.push('\n');
            s.push_str("            }\n");
        }
        s.push_str("            _ => Ordering::Equal,\n");
        s.push_str("        })\n    }\n}\n");
        s
    }

    /// Payload types of a variant plus box flags (record payload = one
    /// unboxed struct field).
    fn variant_tys(&self, v: &CirVariant) -> Vec<(CirType, bool)> {
        match &v.payload {
            CirVariantPayload::None => vec![],
            CirVariantPayload::Tuple(ts, boxed) => {
                ts.iter().cloned().zip(boxed.iter().copied()).collect()
            }
            CirVariantPayload::Record(r) => vec![(CirType::Record(r.clone()), false)],
        }
    }
}

fn contains_fun_or_writeop(t: &CirType) -> bool {
    match t {
        CirType::Fun(..) | CirType::WriteOp => true,
        CirType::Option(t) | CirType::Vector(t) | CirType::Set(t) | CirType::Bag(t) => {
            contains_fun_or_writeop(t)
        }
        CirType::Map(k, v) => contains_fun_or_writeop(k) || contains_fun_or_writeop(v),
        CirType::Tuple(ts) => ts.iter().any(contains_fun_or_writeop),
        _ => false,
    }
}

/// Element type of a collection (or the option payload).
fn elem_ty(t: &CirType) -> CirType {
    match t {
        CirType::Vector(t) | CirType::Set(t) | CirType::Bag(t) | CirType::Option(t) => {
            (**t).clone()
        }
        other => other.clone(),
    }
}

/// Key/value types of a map.
fn map_kv(t: &CirType) -> (CirType, CirType) {
    match t {
        CirType::Map(k, v) => ((**k).clone(), (**v).clone()),
        _ => (CirType::Tuple(vec![]), CirType::Tuple(vec![])),
    }
}

/// Return type of a function-typed CIR expression.
fn fun_ret(t: &CirType) -> CirType {
    match t {
        CirType::Fun(_, r) => (**r).clone(),
        _ => CirType::Tuple(vec![]),
    }
}

// ---------------------------------------------------------------------------
// State / table emission
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    fn emit_state(&mut self) -> String {
        let mut s = String::new();
        for t in &self.cir.tables {
            self.check_no_fun_fields(&format!("table `{}`", t.name), &t.fields);
            s.push_str(&self.key_struct(t));
            s.push_str(&self.record_struct(&t.row, &t.fields));
            s.push_str(&self.table_helpers(t));
        }
        // The typed table state.
        s.push_str("pub struct State {\n");
        for t in &self.cir.tables {
            s.push_str(&format!(
                "    pub {}: MemTable<{}, {}>,\n",
                sanitize(&t.name),
                t.key,
                t.row
            ));
        }
        s.push_str("}\n");
        s.push_str("impl State {\n");
        s.push_str("    pub fn new() -> State {\n        State {\n");
        for t in &self.cir.tables {
            s.push_str(&format!("            {}: MemTable::new(),\n", sanitize(&t.name)));
        }
        s.push_str("        }\n    }\n");
        // Atomic write-op application through the erased runtime registry.
        s.push_str(
            "    /// Apply a set of write_ops atomically (§5.2): clone-apply-check-swap.\n",
        );
        s.push_str("    pub fn apply(&mut self, ops: &CqlSet<WriteOp>) -> Result<(), ApplyError> {\n");
        s.push_str("        let mut __reg = self.__to_registry();\n");
        s.push_str("        let __ops: BTreeSet<WriteOp> = ops.as_slice().iter().cloned().collect();\n");
        s.push_str("        apply_write_ops(&mut __reg, &__ops)?;\n");
        s.push_str("        *self = State::__from_registry(&__reg);\n");
        s.push_str("        Ok(())\n    }\n");
        // State -> erased registry (rows + fks + invariant hooks).
        s.push_str("    fn __to_registry(&self) -> TableRegistry {\n");
        s.push_str("        let mut __reg = TableRegistry::new();\n");
        for t in &self.cir.tables {
            let cols: Vec<String> = t.pk.iter().map(|c| format!("{c:?}.to_string()")).collect();
            s.push_str(&format!(
                "        __reg.register_table(tref_{}(), vec![{}]);\n",
                sanitize(&t.name),
                cols.join(", ")
            ));
        }
        for t in &self.cir.tables {
            for (cols, to) in &t.fks {
                let cols: Vec<String> = cols.iter().map(|c| format!("{c:?}.to_string()")).collect();
                s.push_str(&format!(
                    "        __reg.add_fk(FkDecl {{ from: tref_{}(), cols: vec![{}], to: tref_{}() }});\n",
                    sanitize(&t.name),
                    cols.join(", "),
                    sanitize(to)
                ));
            }
        }
        for inv in &self.cir.invariants {
            s.push_str(&format!(
                "        __reg.add_invariant({:?}, |__reg| {{\n            let __st = State::__from_registry(__reg);\n            __invariant_{}(&__st)\n        }});\n",
                inv.name,
                sanitize(&inv.name)
            ));
        }
        for t in &self.cir.tables {
            s.push_str(&format!(
                "        for (_, __r) in self.{}.scan_all() {{\n            __reg.insert_row(&tref_{}(), {}_row_to_value(__r));\n        }}\n",
                sanitize(&t.name),
                sanitize(&t.name),
                sanitize(&t.name)
            ));
        }
        s.push_str("        __reg\n    }\n");
        // Erased registry -> typed state.
        s.push_str("    fn __from_registry(__reg: &TableRegistry) -> State {\n");
        s.push_str("        let mut __st = State::new();\n");
        for t in &self.cir.tables {
            s.push_str(&format!(
                "        for (_, __v) in __reg.scan({}) {{\n            let __r = {}_row_from_value(__v);\n            let __k = {}_key_of(&__r);\n            __st.{}.insert(__k, __r);\n        }}\n",
                t.id,
                sanitize(&t.name),
                sanitize(&t.name),
                sanitize(&t.name)
            ));
        }
        s.push_str("        __st\n    }\n");
        s.push_str("}\n");
        s
    }

    /// Primary-key struct: `Ord` (BTreeMap key) via canonical order.
    fn key_struct(&self, t: &CirTable) -> String {
        let kfields: Vec<(String, CirType)> = t
            .pk
            .iter()
            .map(|c| {
                let ty = t
                    .fields
                    .iter()
                    .find(|(n, _)| n == c)
                    .map(|(_, ty)| ty.clone())
                    .expect("pk column exists");
                (c.clone(), ty)
            })
            .collect();
        let mut s = String::new();
        s.push_str("#[derive(Debug, Clone, PartialEq, Eq, Hash)]\n");
        s.push_str(&format!("pub struct {} {{\n", t.key));
        for (f, ty) in &kfields {
            s.push_str(&format!("    pub {}: {},\n", sanitize(f), self.ty_str(ty)));
        }
        s.push_str("}\n");
        s.push_str(&format!("impl CanonOrd for {} {{\n", t.key));
        s.push_str("    fn canon_cmp(&self, other: &Self) -> Ordering {\n");
        s.push_str(&self.canon_chain(&kfields, "self", "other"));
        s.push_str("    }\n}\n");
        s.push_str(&format!(
            "impl Ord for {} {{\n    fn cmp(&self, other: &Self) -> Ordering {{\n        self.canon_cmp(other)\n    }}\n}}\n",
            t.key
        ));
        s.push_str(&format!(
            "impl PartialOrd for {} {{\n    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {{\n        Some(self.cmp(other))\n    }}\n}}\n",
            t.key
        ));
        s
    }

    /// `tref_<table>()`, `<table>_key_of`, row <-> Value conversion.
    fn table_helpers(&mut self, t: &CirTable) -> String {
        let n = sanitize(&t.name);
        let mut s = String::new();
        s.push_str(&format!(
            "fn tref_{n}() -> TableRef {{\n    TableRef::new({}, {:?})\n}}\n",
            t.id, t.name
        ));
        s.push_str(&format!("fn {n}_key_of(__r: &{}) -> {} {{\n    {} {{\n", t.row, t.key, t.key));
        for c in &t.pk {
            s.push_str(&format!("        {}: Clone::clone(&__r.{}),\n", sanitize(c), sanitize(c)));
        }
        s.push_str("    }\n}\n");
        // Row -> Value::Record (all columns; the registry extracts the key).
        s.push_str(&format!("fn {n}_row_to_value(__r: &{}) -> Value {{\n", t.row));
        s.push_str("    let mut __m = BTreeMap::new();\n");
        for (f, ty) in &t.fields {
            s.push_str(&format!(
                "    __m.insert({:?}.to_string(), {});\n",
                f,
                self.to_value(&format!("Clone::clone(&__r.{})", sanitize(f)), ty)
            ));
        }
        s.push_str("    Value::Record(__m)\n}\n");
        // Value -> row.
        s.push_str(&format!("fn {n}_row_from_value(__v: &Value) -> {} {{\n", t.row));
        s.push_str("    match __v {\n        Value::Record(__m) => {\n");
        s.push_str("            let __get = |__k: &str| -> &Value { __m.get(__k).unwrap_or_else(|| cql_trap_msg(\"row is missing a column\")) };\n");
        s.push_str(&format!("            {} {{\n", t.row));
        for (f, ty) in &t.fields {
            s.push_str(&format!(
                "                {}: {},\n",
                sanitize(f),
                self.from_value(&format!("__get({f:?})"), ty)
            ));
        }
        s.push_str("            }\n        }\n");
        s.push_str("        _ => cql_trap_msg(\"expected a row record value\"),\n    }\n}\n");
        s
    }
}

// ---------------------------------------------------------------------------
// Function / test emission
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    fn emit_functions(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.cir.consts {
            out.push(format!(
                "pub fn cql_const_{}() -> {} {{\n    {}\n}}\n",
                sanitize(&c.name),
                self.ty_str(&c.ty),
                self.expr(&c.value)
            ));
        }
        for inv in &self.cir.invariants {
            out.push(format!(
                "fn __invariant_{}(state: &State) -> bool {{\n    {}\n}}\n",
                sanitize(&inv.name),
                self.expr(&inv.body)
            ));
        }
        for f in &self.cir.functions {
            match &f.env {
                Some(fields) => {
                    // Lifted lambda: explicit env struct + (env, arg) signature.
                    // Functions whose body touches tables take `state` first.
                    let mut s = String::new();
                    s.push_str(&format!("struct {}_Env {{\n", f.name));
                    for (n, t) in fields {
                        s.push_str(&format!("    {}: {},\n", n, self.ty_str(t)));
                    }
                    s.push_str("}\n");
                    let (_, pty) = &f.params[0];
                    let state_param = if self.fn_state.get(&f.name).copied().unwrap_or(false) {
                        "state: &State, "
                    } else {
                        ""
                    };
                    s.push_str(&format!(
                        "fn {}({}env: &{}_Env, __arg: {}) -> {} {{\n    {}\n}}\n",
                        f.name,
                        state_param,
                        f.name,
                        self.ty_str(pty),
                        self.ty_str(&f.ret),
                        self.expr(&f.body)
                    ));
                    out.push(s);
                }
                None => {
                    let params: Vec<String> = f
                        .params
                        .iter()
                        .map(|(n, t)| format!("{}: {}", n, self.ty_str(t)))
                        .collect();
                    out.push(format!(
                        "pub fn {}({}) -> {} {{\n    {}\n}}\n",
                        f.name,
                        params.join(", "),
                        self.ty_str(&f.ret),
                        self.expr(&f.body)
                    ));
                }
            }
        }
        for op in &self.cir.operators {
            let mut params: Vec<String> = vec!["state: &State".to_string()];
            params.extend(
                op.params
                    .iter()
                    .map(|(n, t)| format!("{}: {}", n, self.ty_str(t))),
            );
            out.push(format!(
                "pub fn {}({}) -> {} {{\n    {}\n}}\n",
                op.name,
                params.join(", "),
                self.ty_str(&op.ret),
                self.expr(&op.body)
            ));
        }
        out
    }

    fn emit_tests(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.cir.tests {
            let mut s = String::new();
            s.push_str(&format!("    #[test]\n    fn test_{}() {{\n", sanitize(&t.name)));
            s.push_str("        let mut state = State::new();\n");
            for (table, rows) in &t.fixtures {
                s.push_str(&format!("        for __r in {} {{\n", self.expr(rows)));
                s.push_str(&format!(
                    "            let __k = {}_key_of(&__r);\n            state.{}.insert(__k, __r);\n        }}\n",
                    sanitize(table),
                    sanitize(table)
                ));
            }
            // Fixtures mutate; expects see a shared reference (closures in
            // read predicates capture `state` as `&State`).
            s.push_str("        let state = &state;\n");
            for (lhs, rhs) in &t.expects {
                s.push_str(&format!(
                    "        assert_eq!({}, {});\n",
                    self.expr(lhs),
                    self.expr(rhs)
                ));
            }
            s.push_str("    }\n");
            out.push(s);
        }
        out
    }

    // -- value conversion (typed Rust <-> erased runtime Value) ----------------

    /// Rust value expression -> runtime `Value`; `code` yields an owned `T`.
    fn to_value(&mut self, code: &str, ty: &CirType) -> String {
        match ty {
            CirType::Bool => format!("Value::Bool({code})"),
            CirType::Int => format!("Value::Int({code})"),
            CirType::Float => format!("Value::Float({code})"),
            CirType::Decimal(_) => format!("Value::Decimal({code})"),
            CirType::String => format!("Value::Str({code})"),
            CirType::Date => format!("Value::Date({code})"),
            CirType::Option(t) => format!(
                "match {code} {{ Some(__v) => Value::Option(Some(Box::new({}))), None => Value::Option(None) }}",
                self.to_value("__v", t)
            ),
            CirType::Tuple(ts) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.to_value(&format!("__t.{i}"), t))
                    .collect();
                format!("{{ let __t = {code}; Value::Tuple(vec![{}]) }}", elems.join(", "))
            }
            CirType::Record(name) => {
                let fields = self.record(name).fields.clone();
                self.record_to_value(code, &fields)
            }
            CirType::Row(name) => {
                let fields = self.table_by_row(name).fields.clone();
                self.record_to_value(code, &fields)
            }
            CirType::Enum(name) => {
                let def = self.enum_def_ref(name).clone();
                let mut arms = String::new();
                for v in &def.variants {
                    let tys = self.variant_tys(v);
                    if tys.is_empty() {
                        arms.push_str(&format!(
                            " {}::{} => Value::Enum {{ variant: {:?}.to_string(), payload: vec![] }},",
                            def.name, v.name, v.name
                        ));
                        continue;
                    }
                    let binds: Vec<String> =
                        (0..tys.len()).map(|i| format!("__v{i}")).collect();
                    let payload: Vec<String> = tys
                        .iter()
                        .enumerate()
                        .map(|(i, (t, b))| {
                            if *b {
                                self.to_value(&format!("*__v{i}"), t)
                            } else {
                                self.to_value(&format!("__v{i}"), t)
                            }
                        })
                        .collect();
                    arms.push_str(&format!(
                        " {}::{}({}) => Value::Enum {{ variant: {:?}.to_string(), payload: vec![{}] }},",
                        def.name,
                        v.name,
                        binds.join(", "),
                        v.name,
                        payload.join(", ")
                    ));
                }
                format!("match {code} {{{arms} }}")
            }
            CirType::Vector(t) => format!(
                "Value::Vector(({code}).into_iter().map(|__v| {}).collect())",
                self.to_value("__v", t)
            ),
            CirType::Set(t) => format!(
                "Value::Set(CqlSet::from_elems(({code}).as_slice().iter().map(|__v| {})))",
                self.to_value_coll("Clone::clone(__v)", t)
            ),
            CirType::Bag(t) => format!(
                "Value::Bag(CqlBag::from_elems(({code}).iter_expanded().map(|__v| {})))",
                self.to_value_coll("Clone::clone(__v)", t)
            ),
            CirType::Map(k, v) => format!(
                "Value::Map(CqlMap::from_vector(({code}).iter().map(|(__k, __v)| ({}, {}))))",
                self.to_value_coll("Clone::clone(__k)", k),
                self.to_value_coll("Clone::clone(__v)", v)
            ),
            CirType::Fun(..) | CirType::WriteOp => {
                self.err("cannot convert a function/write_op value to a runtime Value");
                "unreachable!()".to_string()
            }
        }
    }

    fn record_to_value(&mut self, code: &str, fields: &[(String, CirType)]) -> String {
        let mut inserts = String::new();
        for (f, ty) in fields {
            inserts.push_str(&format!(
                " __m.insert({:?}.to_string(), {});",
                f,
                self.to_value(&format!("__r.{}", sanitize(f)), ty)
            ));
        }
        format!("{{ let __r = {code}; let mut __m = BTreeMap::new();{inserts} Value::Record(__m) }}")
    }

    /// Runtime `Value` -> Rust value; `code` yields a `&Value`.
    fn from_value(&mut self, code: &str, ty: &CirType) -> String {
        match ty {
            CirType::Bool => format!(
                "match {code} {{ Value::Bool(__v) => *__v, _ => cql_trap_msg(\"bad value: expected bool\") }}"
            ),
            CirType::Int => format!(
                "match {code} {{ Value::Int(__v) => *__v, _ => cql_trap_msg(\"bad value: expected int\") }}"
            ),
            CirType::Float => format!(
                "match {code} {{ Value::Float(__v) => *__v, _ => cql_trap_msg(\"bad value: expected float\") }}"
            ),
            CirType::Decimal(_) => format!(
                "match {code} {{ Value::Decimal(__v) => *__v, _ => cql_trap_msg(\"bad value: expected decimal\") }}"
            ),
            CirType::String => format!(
                "match {code} {{ Value::Str(__v) => Clone::clone(__v), _ => cql_trap_msg(\"bad value: expected string\") }}"
            ),
            CirType::Date => format!(
                "match {code} {{ Value::Date(__v) => *__v, _ => cql_trap_msg(\"bad value: expected date\") }}"
            ),
            CirType::Option(t) => format!(
                "match {code} {{ Value::Option(None) => None, Value::Option(Some(__v)) => Some({}), _ => cql_trap_msg(\"bad value: expected option\") }}",
                self.from_value("&**__v", t)
            ),
            CirType::Tuple(ts) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.from_value(&format!("&__vs[{i}]"), t))
                    .collect();
                format!(
                    "match {code} {{ Value::Tuple(__vs) if __vs.len() == {} => ({}), _ => cql_trap_msg(\"bad value: expected tuple\") }}",
                    ts.len(),
                    elems.join(", ")
                )
            }
            CirType::Record(name) => {
                let fields = self.record(name).fields.clone();
                self.record_from_value(code, name, &fields)
            }
            CirType::Row(name) => {
                let fields = self.table_by_row(name).fields.clone();
                self.record_from_value(code, name, &fields)
            }
            CirType::Enum(name) => {
                let def = self.enum_def_ref(name).clone();
                let mut arms = String::new();
                for v in &def.variants {
                    let tys = self.variant_tys(v);
                    if tys.is_empty() {
                        arms.push_str(&format!(" {:?} => {}::{},", v.name, def.name, v.name));
                        continue;
                    }
                    let args: Vec<String> = tys
                        .iter()
                        .enumerate()
                        .map(|(i, (t, b))| {
                            let inner = self.from_value(&format!("&__p[{i}]"), t);
                            if *b {
                                format!("Box::new({inner})")
                            } else {
                                inner
                            }
                        })
                        .collect();
                    arms.push_str(&format!(
                        " {:?} if __p.len() == {} => {}::{}({}),",
                        v.name,
                        tys.len(),
                        def.name,
                        v.name,
                        args.join(", ")
                    ));
                }
                format!(
                    "match {code} {{ Value::Enum {{ variant: __n, payload: __p }} => match __n.as_str() {{{arms} _ => cql_trap_msg(\"bad value: unknown enum variant\") }}, _ => cql_trap_msg(\"bad value: expected enum\") }}"
                )
            }
            CirType::Vector(t) => format!(
                "match {code} {{ Value::Vector(__vs) => __vs.iter().map(|__v| {}).collect(), _ => cql_trap_msg(\"bad value: expected vector\") }}",
                self.from_value("__v", t)
            ),
            CirType::Set(t) => format!(
                "match {code} {{ Value::Set(__s) => CqlSet::from_elems(__s.as_slice().iter().map(|__v| {})), _ => cql_trap_msg(\"bad value: expected set\") }}",
                self.from_value_coll("__v", t)
            ),
            CirType::Bag(t) => format!(
                "match {code} {{ Value::Bag(__b) => CqlBag::from_elems(__b.iter_expanded().map(|__v| {})), _ => cql_trap_msg(\"bad value: expected bag\") }}",
                self.from_value_coll("__v", t)
            ),
            CirType::Map(k, v) => format!(
                "match {code} {{ Value::Map(__m) => CqlMap::from_vector(__m.iter().map(|(__k, __v)| ({}, {}))), _ => cql_trap_msg(\"bad value: expected map\") }}",
                self.from_value_coll("__k", k),
                self.from_value_coll("__v", v)
            ),
            CirType::Fun(..) | CirType::WriteOp => {
                self.err("cannot convert a runtime Value to a function/write_op");
                "unreachable!()".to_string()
            }
        }
    }

    fn record_from_value(
        &mut self,
        code: &str,
        name: &str,
        fields: &[(String, CirType)],
    ) -> String {
        let mut inits = String::new();
        for (f, ty) in fields {
            inits.push_str(&format!(
                " {}: {},",
                sanitize(f),
                self.from_value(&format!("__get({f:?})"), ty)
            ));
        }
        format!(
            "match {code} {{ Value::Record(__m) => {{ let __get = |__k: &str| -> &Value {{ __m.get(__k).unwrap_or_else(|| cql_trap_msg(\"record is missing a field\")) }}; {name} {{{inits} }} }}, _ => cql_trap_msg(\"bad value: expected record\") }}"
        )
    }

    fn enum_def_ref(&self, name: &str) -> &CirEnumDef {
        self.enum_def(name)
    }
}

// ---------------------------------------------------------------------------
// Expression emission
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    fn expr(&mut self, e: &CirExpr) -> String {
        match &e.kind {
            CirExprKind::Lit(l) => self.lit(l),
            // Locals/captures are owned values; clone at every use site so
            // repeated uses never hit move-out errors (MVP: correctness over
            // clone count).
            CirExprKind::Var(n) => format!("Clone::clone(&{n})"),
            CirExprKind::EnvGet(n) => format!("Clone::clone(&env.{n})"),
            CirExprKind::ConstRef(n) => match n.rsplit_once("::") {
                // Cross-module reference: `crate::<mod>::cql_const_<name>()`.
                Some((path, base)) => format!("{}::cql_const_{}()", path, sanitize(base)),
                None => format!("cql_const_{}()", sanitize(n)),
            },
            CirExprKind::FunRef { name } => self.fun_ref(name),
            CirExprKind::StdLibRef { name } => self.stdlib_ref(name, e),
            CirExprKind::Deref(inner) => format!("*{}", self.expr(inner)),
            CirExprKind::MakeClosure { fun, env } => {
                let fields: Vec<String> = env
                    .iter()
                    .map(|(n, x)| format!("{}: {}", n, self.expr(x)))
                    .collect();
                // Lifted functions whose body touches tables take the state
                // as an extra first parameter; the closure captures it.
                let call = if self.fn_state.get(fun).copied().unwrap_or(false) {
                    format!("{fun}(state, &__env, __arg)")
                } else {
                    format!("{fun}(&__env, __arg)")
                };
                format!(
                    "{{ let __env = {fun}_Env {{ {} }}; Rc::new(move |__arg| {call}) }}",
                    fields.join(", ")
                )
            }
            CirExprKind::Call { callee, args } => match callee {
                CirCallee::Operator { name, level } => {
                    let mut a: Vec<String> = Vec::new();
                    if *level != EffectLevel::Function {
                        // L1/L2 operators take the table state first.
                        a.push("&state".to_string());
                    }
                    a.extend(args.iter().map(|x| self.expr(x)));
                    format!("{}({})", name, a.join(", "))
                }
                CirCallee::StdLib { name } => self.stdlib_call(name, args, e),
            },
            CirExprKind::App { func, args } => {
                let f = self.expr(func);
                let packed = self.pack_args(args);
                format!("({f})({packed})")
            }
            CirExprKind::Let { pat, value, body } => {
                let v = self.expr(value);
                let b = self.expr(body);
                let (p, guard) = self.pat_guarded(pat);
                if guard.is_none() && is_irrefutable(pat) {
                    format!("{{ let {p}: {} = {v}; {b} }}", self.ty_str(&value.ty))
                } else {
                    // Refutable let: trap on mismatch (§5.3).
                    let g = guard.map(|g| format!(" if {g}")).unwrap_or_default();
                    format!(
                        "{{ let __rv = {v}; match __rv {{ {p}{g} => {b}, _ => cql_trap_msg(\"refutable pattern failed\") }} }}"
                    )
                }
            }
            CirExprKind::If {
                cond,
                then_br,
                else_br,
            } => format!(
                "if {} {{ {} }} else {{ {} }}",
                self.expr(cond),
                self.expr(then_br),
                self.expr(else_br)
            ),
            CirExprKind::Match { scrutinee, arms } => {
                let scr = self.expr(scrutinee);
                let mut s = format!("match {scr} {{\n");
                let mut exhaustive = false;
                let mut any_guard = false;
                for arm in arms {
                    let (p, guard) = self.pat_guarded(&arm.pat);
                    let g = guard
                        .as_ref()
                        .map(|g| format!(" if {g}"))
                        .unwrap_or_default();
                    s.push_str(&format!("    {p}{g} => {},\n", self.expr(&arm.body)));
                    if guard.is_none() && is_irrefutable(&arm.pat) {
                        exhaustive = true;
                    }
                    if guard.is_some() {
                        any_guard = true;
                    }
                }
                // Unguarded arms covering every enum variant are exhaustive.
                let covers_all = !any_guard
                    && match &scrutinee.ty {
                        CirType::Enum(n) => {
                            let def = self.enum_def(n);
                            let covered: HashSet<&str> = arms
                                .iter()
                                .filter_map(|a| match &a.pat {
                                    CirPat::Variant { variant, .. } => Some(variant.as_str()),
                                    _ => None,
                                })
                                .collect();
                            covered.len() == arms.len()
                                && def.variants.iter().all(|v| covered.contains(v.name.as_str()))
                        }
                        _ => false,
                    };
                if !exhaustive && !covers_all {
                    s.push_str("    _ => cql_trap_msg(\"non-exhaustive match\"),\n");
                }
                s.push('}');
                s
            }
            CirExprKind::RecordLit { def, fields } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(n, x)| format!("{}: {}", sanitize(n), self.expr(x)))
                    .collect();
                format!("{} {{ {} }}", def, fs.join(", "))
            }
            CirExprKind::RecordUpd { def, base, fields } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(n, x)| format!("{}: {}", sanitize(n), self.expr(x)))
                    .collect();
                format!(
                    "{} {{ {}, ..{} }}",
                    def,
                    fs.join(", "),
                    self.expr(base)
                )
            }
            CirExprKind::Tuple(xs) => {
                let es: Vec<String> = xs.iter().map(|x| self.expr(x)).collect();
                format!("({})", es.join(", "))
            }
            CirExprKind::Vector(xs) => {
                if xs.is_empty() {
                    let t = match &e.ty {
                        CirType::Vector(t) => self.ty_str(t),
                        _ => unreachable!(),
                    };
                    format!("{{ let __e: Vec<{t}> = Vec::new(); __e }}")
                } else {
                    let es: Vec<String> = xs.iter().map(|x| self.expr(x)).collect();
                    format!("vec![{}]", es.join(", "))
                }
            }
            CirExprKind::Set(xs) => {
                let elem = match &e.ty {
                    CirType::Set(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                if xs.is_empty() {
                    format!("CqlSet::<{}>::new()", self.coll_ty_str(&elem))
                } else {
                    let es: Vec<String> = xs
                        .iter()
                        .map(|x| {
                            let c = self.expr(x);
                            self.wrap(&c, &elem)
                        })
                        .collect();
                    format!("CqlSet::from_elems(vec![{}])", es.join(", "))
                }
            }
            CirExprKind::Bag(xs) => {
                let elem = match &e.ty {
                    CirType::Bag(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                if xs.is_empty() {
                    format!("CqlBag::<{}>::new()", self.coll_ty_str(&elem))
                } else {
                    let es: Vec<String> = xs
                        .iter()
                        .map(|x| {
                            let c = self.expr(x);
                            self.wrap(&c, &elem)
                        })
                        .collect();
                    format!("CqlBag::from_elems(vec![{}])", es.join(", "))
                }
            }
            CirExprKind::MapLit(kvs) => {
                let (kt, vt) = match &e.ty {
                    CirType::Map(k, v) => ((**k).clone(), (**v).clone()),
                    _ => unreachable!(),
                };
                if kvs.is_empty() {
                    format!(
                        "CqlMap::<{}, {}>::new()",
                        self.coll_ty_str(&kt),
                        self.coll_ty_str(&vt)
                    )
                } else {
                    let es: Vec<String> = kvs
                        .iter()
                        .map(|(k, v)| {
                            let k = self.expr(k);
                            let v = self.expr(v);
                            format!("({}, {})", self.wrap(&k, &kt), self.wrap(&v, &vt))
                        })
                        .collect();
                    format!("CqlMap::from_vector(vec![{}])", es.join(", "))
                }
            }
            CirExprKind::OptionSome(x) => format!("Some({})", self.expr(x)),
            CirExprKind::OptionNone => match &e.ty {
                CirType::Option(t) => format!("Option::<{}>::None", self.ty_str(t)),
                _ => "None".to_string(),
            },
            CirExprKind::BinOp { op, lhs, rhs } => self.binop(*op, lhs, rhs),
            CirExprKind::UnOp { op, operand } => {
                let x = self.expr(operand);
                match op {
                    UnOpKind::Not => format!("!({x})"),
                    UnOpKind::Neg => match &operand.ty {
                        CirType::Int => format!("cql_trap(checked_neg({x}))"),
                        CirType::Float => format!("-({x})"),
                        CirType::Decimal(_) => format!("({x}).neg()"),
                        _ => {
                            self.err("unary minus on a non-numeric value");
                            x
                        }
                    },
                }
            }
            CirExprKind::Field { base, name } => {
                format!("({}).{}", self.expr(base), sanitize(name))
            }
            CirExprKind::TupleProj { base, index } => {
                format!("({}).{index}", self.expr(base))
            }
            CirExprKind::EnumConstruct { def, variant, args } => {
                let v = self.variant(def, variant);
                let tys = self.variant_tys(v);
                let es: Vec<String> = args
                    .iter()
                    .enumerate()
                    .map(|(i, x)| {
                        let code = self.expr(x);
                        if tys.get(i).map(|(_, b)| *b).unwrap_or(false) {
                            format!("Box::new({code})")
                        } else {
                            code
                        }
                    })
                    .collect();
                format!("{def}::{variant}({})", es.join(", "))
            }
            CirExprKind::Cast { target, expr } => self.cast(expr, target),
            CirExprKind::Read {
                table,
                plan,
                key,
                predicate,
            } => self.read(table, plan, key, predicate),
            CirExprKind::WriteOp(w) => self.write_op(w),
        }
    }

    fn lit(&self, l: &CirLit) -> String {
        match l {
            CirLit::Bool(b) => b.to_string(),
            CirLit::Int(i) => format!("{i}i64"),
            CirLit::Float(f) => format!("{f:?}f64"),
            CirLit::Str(s) => format!("{s:?}.to_string()"),
            CirLit::Date { year, month, day } => {
                format!("Date::new({year}, {month}, {day}).expect(\"valid date literal\")")
            }
            CirLit::Decimal { repr, precision } => {
                let p = match precision {
                    Some((m, n)) => format!("Some(({m}, {n}))"),
                    None => "None".to_string(),
                };
                format!(
                    "stdlib::decimal_from_string({repr:?}, {p}).expect(\"valid decimal literal\")"
                )
            }
        }
    }

    /// A plain L0 function used as a value: wrap in an `Rc<dyn Fn>` adapter.
    fn fun_ref(&mut self, name: &str) -> String {
        if name.contains("::") {
            // Cross-module reference (single-argument imported function).
            return format!("Rc::new(|__arg| {name}(__arg))");
        }
        let f = self
            .cir
            .functions
            .iter()
            .find(|f| f.name == name && f.env.is_none());
        match f {
            Some(f) => {
                if f.params.len() <= 1 {
                    format!("Rc::new(|__arg| {name}(__arg))")
                } else {
                    let binds: Vec<String> =
                        (0..f.params.len()).map(|i| format!("__a{i}")).collect();
                    format!(
                        "Rc::new(|__arg| {{ let ({}) = __arg; {name}({}) }})",
                        binds.join(", "),
                        binds.join(", ")
                    )
                }
            }
            None => {
                self.err(format!("first-class reference to unknown function `{name}`"));
                "unreachable!()".to_string()
            }
        }
    }

    /// A single-argument stdlib function used as a value.
    fn stdlib_ref(&mut self, name: &str, e: &CirExpr) -> String {
        let body: Option<String> = match name {
            "to_string_int" => Some("stdlib::to_string_int(__arg)".into()),
            "to_string_float" => Some("stdlib::to_string_float(__arg)".into()),
            "to_string_bool" => Some("stdlib::to_string_bool(__arg)".into()),
            "to_string_date" => Some("stdlib::to_string_date(&__arg)".into()),
            "to_string_decimal" => Some("stdlib::to_string_decimal(&__arg)".into()),
            "length" => Some("stdlib::str_length(&__arg)".into()),
            "trim" => Some("stdlib::trim(&__arg)".into()),
            "year" => Some("stdlib::year(&__arg)".into()),
            "month" => Some("stdlib::month(&__arg)".into()),
            "day" => Some("stdlib::day(&__arg)".into()),
            "day_of_week" => Some("stdlib::day_of_week(&__arg)".into()),
            "abs" => Some("cql_trap(stdlib::abs(__arg))".into()),
            "floor" => Some("stdlib::floor(__arg)".into()),
            "ceil" => Some("stdlib::ceil(__arg)".into()),
            "round" => Some("stdlib::round(__arg)".into()),
            "is_some" => Some("stdlib::is_some(&__arg)".into()),
            "is_none" => Some("stdlib::is_none(&__arg)".into()),
            "parse_date" => Some("stdlib::parse_date(&__arg)".into()),
            _ => None,
        };
        match body {
            Some(b) => format!("Rc::new(move |__arg| {b})"),
            None => {
                self.err(format!(
                    "first-class reference to stdlib `{name}` is not supported by the Rust backend"
                ));
                let _ = e;
                "unreachable!()".to_string()
            }
        }
    }

    /// Pack call arguments into the single function-value argument (tuple
    /// for multi-parameter functions).
    fn pack_args(&mut self, args: &[CirExpr]) -> String {
        if args.len() == 1 {
            self.expr(&args[0])
        } else {
            let es: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
            format!("({})", es.join(", "))
        }
    }

    fn cast(&mut self, inner: &CirExpr, target: &CirType) -> String {
        let x = self.expr(inner);
        match (&inner.ty, target) {
            (CirType::Int, CirType::Float) => format!("int_as_float({x})"),
            (CirType::Int, CirType::Decimal(Some((m, n)))) => {
                format!("cql_trap(int_as_decimal({m}, {n}, {x}))")
            }
            (CirType::Float, CirType::Int) => format!("cql_trap(float_as_int({x}))"),
            (CirType::Decimal(_), CirType::Int) => format!("cql_trap(({x}).as_int())"),
            (CirType::Decimal(_), CirType::Float) => format!("({x}).as_float()"),
            (CirType::Decimal(_), CirType::Decimal(None)) => format!("({x}).as_unbounded()"),
            (CirType::Decimal(_), CirType::Decimal(Some((m, n)))) => {
                format!("cql_trap(({x}).as_bounded({m}, {n}))")
            }
            (a, b) if a == b => format!("({x})"),
            _ => {
                self.err(format!(
                    "unsupported cast from {:?} to {:?} (§2.4 whitelist)",
                    inner.ty, target
                ));
                x
            }
        }
    }

    /// A table read with the optimizer's plan (§5.5). IndexScan is compiled
    /// as a filtered full scan (documented MVP deviation; the runtime keeps
    /// the `IndexedTable` interface for later).
    fn read(
        &mut self,
        table: &str,
        plan: &ReadPlan,
        key: &[(String, CirExpr)],
        predicate: &CirExpr,
    ) -> String {
        let t = self.table(table);
        let (tname, trow, tkey, tpk) =
            (sanitize(&t.name), t.row.clone(), t.key.clone(), t.pk.clone());
        let pred = self.expr(predicate);
        let point = matches!(plan, ReadPlan::PointLookup)
            && key.len() == tpk.len()
            && tpk.iter().all(|c| key.iter().any(|(k, _)| k == c));
        let rows = if point {
            let kvs: Vec<String> = key
                .iter()
                .map(|(c, x)| {
                    let x = self.expr(x);
                    format!("{}: {}", sanitize(c), x)
                })
                .collect();
            format!(
                "{{ let __key = {} {{ {} }}; match state.{}.lookup(&__key) {{ Some(__r) if (__pred)(Clone::clone(__r)) => vec![Clone::clone(__r)], _ => vec![] }} }}",
                tkey,
                kvs.join(", "),
                tname
            )
        } else {
            format!(
                "state.{}.scan_all().filter_map(|(_, __r)| if (__pred)(Clone::clone(__r)) {{ Some(Clone::clone(__r)) }} else {{ None }}).collect()",
                tname
            )
        };
        format!(
            "{{ let __pred = {pred}; let __rows: Vec<{}> = {rows}; CqlSet::from_elems(__rows) }}",
            trow
        )
    }

    fn write_op(&mut self, w: &CirWriteOp) -> String {
        match w {
            CirWriteOp::Insert { table, row } => {
                let n = sanitize(table);
                format!(
                    "WriteOp::Insert {{ table: tref_{n}(), row: {n}_row_to_value(&({})) }}",
                    self.expr(row)
                )
            }
            CirWriteOp::Delete { table, key } => {
                let n = sanitize(table);
                let k = self.expr(key);
                let kv = self.to_value(&k, &key.ty);
                format!("WriteOp::Delete {{ table: tref_{n}(), key: {kv} }}")
            }
            CirWriteOp::Update {
                table,
                key,
                transform,
                def_id,
            } => {
                let n = sanitize(table);
                let k = self.expr(key);
                let kv = self.to_value(&k, &key.ty);
                let tr = self.expr(transform);
                // The CQL transform maps the non-key *value record* to a new
                // value record; the runtime hands us the full row Value, so
                // adapt: value-record in, merge transformed fields with the
                // original key columns, full row out.
                let vrec = match &transform.ty {
                    CirType::Fun(arg, _) => (**arg).clone(),
                    _ => {
                        self.err("update transform is not a function");
                        CirType::Tuple(vec![])
                    }
                };
                let (fields, pk) = {
                    let t = self.table(table);
                    (t.fields.clone(), t.pk.clone())
                };
                let from = self.from_value("__v", &vrec);
                let mut merge = String::from(
                    "{ let __kr = match __v { Value::Record(__m) => __m, _ => cql_trap_msg(\"expected a row record value\") };",
                );
                merge.push_str(&format!(" let __new = ({tr})({from});"));
                merge.push_str(" let mut __m = BTreeMap::new();");
                for (f, ty) in &fields {
                    if pk.contains(f) {
                        merge.push_str(&format!(
                            " __m.insert({f:?}.to_string(), Clone::clone(__kr.get({f:?}).unwrap_or_else(|| cql_trap_msg(\"row is missing a key column\"))));"
                        ));
                    } else {
                        let v = self.to_value(&format!("Clone::clone(&__new.{})", sanitize(f)), ty);
                        merge.push_str(&format!(" __m.insert({f:?}.to_string(), {v});"));
                    }
                }
                merge.push_str(" Value::Record(__m) }");
                // MVP deviation: the transform's environment is already
                // inside its Rc closure, so `captures` is reported as empty
                // (§3.6 capture lists are not emitted yet).
                format!(
                    "WriteOp::Update {{ table: tref_{n}(), key: {kv}, transform: Arc::new(ClosureFunVal::new({def_id}, vec![], move |__v: &Value| {merge})) }}"
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    /// Emit a pattern plus an optional guard (string literals cannot be
    /// Rust patterns; they become bindings with an equality guard).
    fn pat_guarded(&mut self, p: &CirPat) -> (String, Option<String>) {
        let mut guards = Vec::new();
        let s = self.pat_g(p, &mut guards);
        let guard = if guards.is_empty() {
            None
        } else {
            Some(guards.join(" && "))
        };
        (s, guard)
    }

    fn pat_g(&mut self, p: &CirPat, guards: &mut Vec<String>) -> String {
        match p {
            CirPat::Wildcard => "_".into(),
            CirPat::Bind(n) => sanitize(n),
            CirPat::Lit(PatLit::Int(i)) => format!("{i}i64"),
            CirPat::Lit(PatLit::Bool(b)) => b.to_string(),
            CirPat::Lit(PatLit::Str(s)) => {
                let n = format!("__pg{}", self.tmp);
                self.tmp += 1;
                guards.push(format!("{n} == {s:?}"));
                n
            }
            CirPat::None => "None".into(),
            CirPat::Some(inner) => format!("Some({})", self.pat_g(inner, guards)),
            CirPat::Variant { def, variant, args } => {
                if args.is_empty() {
                    format!("{def}::{variant}")
                } else {
                    let ps: Vec<String> =
                        args.iter().map(|a| self.pat_g(a, guards)).collect();
                    format!("{def}::{variant}({})", ps.join(", "))
                }
            }
            CirPat::Tuple(ps) => {
                let ps: Vec<String> = ps.iter().map(|x| self.pat_g(x, guards)).collect();
                format!("({})", ps.join(", "))
            }
            CirPat::Record { def, fields } => {
                let fs: Vec<String> = fields.iter().map(|f| sanitize(f)).collect();
                format!("{def} {{ {}, .. }}", fs.join(", "))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Operators and stdlib calls
// ---------------------------------------------------------------------------

impl<'a> Emitter<'a> {
    fn binop(&mut self, op: BinOpKind, lhs: &CirExpr, rhs: &CirExpr) -> String {
        let a = self.expr(lhs);
        let b = self.expr(rhs);
        match op {
            BinOpKind::Add | BinOpKind::Sub | BinOpKind::Mul | BinOpKind::Div | BinOpKind::Mod => {
                match &lhs.ty {
                    CirType::Int => {
                        let f = match op {
                            BinOpKind::Add => "checked_add",
                            BinOpKind::Sub => "checked_sub",
                            BinOpKind::Mul => "checked_mul",
                            BinOpKind::Div => "checked_div",
                            _ => "checked_rem",
                        };
                        format!("cql_trap({f}({a}, {b}))")
                    }
                    CirType::Float => {
                        let o = match op {
                            BinOpKind::Add => "+",
                            BinOpKind::Sub => "-",
                            BinOpKind::Mul => "*",
                            BinOpKind::Div => "/",
                            _ => "%",
                        };
                        format!("({a}) {o} ({b})")
                    }
                    CirType::Decimal(_) => {
                        let m = match op {
                            BinOpKind::Add => "add",
                            BinOpKind::Sub => "sub",
                            BinOpKind::Mul => "mul",
                            BinOpKind::Div => "div",
                            _ => {
                                self.err("`%` on decimals is not supported");
                                "add"
                            }
                        };
                        format!("cql_trap(({a}).{m}(&({b})))")
                    }
                    CirType::String if matches!(op, BinOpKind::Add) => {
                        format!("stdlib::concat(&({a}), &({b}))")
                    }
                    _ => {
                        self.err(format!("arithmetic on {:?} is not supported", lhs.ty));
                        format!("({a})")
                    }
                }
            }
            BinOpKind::Eq => format!("({a}) == ({b})"),
            BinOpKind::Ne => format!("({a}) != ({b})"),
            BinOpKind::Lt | BinOpKind::Gt | BinOpKind::Le | BinOpKind::Ge => {
                if matches!(lhs.ty, CirType::Float) {
                    let o = match op {
                        BinOpKind::Lt => "<",
                        BinOpKind::Gt => ">",
                        BinOpKind::Le => "<=",
                        _ => ">=",
                    };
                    format!("({a}) {o} ({b})")
                } else {
                    let ord = match op {
                        BinOpKind::Lt => "Ordering::Less",
                        BinOpKind::Gt => "Ordering::Greater",
                        BinOpKind::Le => "Ordering::Less | Ordering::Equal",
                        _ => "Ordering::Greater | Ordering::Equal",
                    };
                    format!("matches!(({a}).canon_cmp(&({b})), {ord})")
                }
            }
            BinOpKind::And => format!("({a}) && ({b})"),
            BinOpKind::Or => format!("({a}) || ({b})"),
            BinOpKind::Impl => format!("!({a}) || ({b})"),
            BinOpKind::In => {
                let elem = elem_ty(&rhs.ty);
                let w = self.wrap(&a, &elem);
                format!("({b}).contains(&({w}))")
            }
            BinOpKind::SubsetEq => format!("({a}).is_subset(&({b}))"),
            BinOpKind::Cup => match &lhs.ty {
                CirType::Set(_) => format!("({a}).union(&({b}))"),
                CirType::Bag(_) => format!("({a}).bag_union(&({b}))"),
                _ => {
                    self.err("`\\cup` on a non-set/non-bag value");
                    format!("({a})")
                }
            },
            BinOpKind::Cap => format!("({a}).inter(&({b}))"),
            BinOpKind::Diff => format!("({a}).diff(&({b}))"),
            BinOpKind::Cartesian => format!("({a}).cartesian(&({b}))"),
        }
    }

    /// `f: Rc<dyn Fn(E) -> R>` → `impl Fn(&coll(E)) -> R`: clones the
    /// element and unwraps collection storage (identity for plain types).
    fn adapt_elem(&mut self, code: &str, elem: &CirType) -> String {
        let arg = self.unwrap("Clone::clone(__x)", elem);
        format!("move |__x| ({code})({arg})")
    }

    /// `f: Rc<dyn Fn((A, E)) -> A>` → `impl Fn(A, &coll(E)) -> A`.
    fn adapt_elem2(&mut self, code: &str, elem: &CirType) -> String {
        let arg = self.unwrap("Clone::clone(__x)", elem);
        format!("move |__a, __x| ({code})((__a, {arg}))")
    }

    /// Aggregate group-key adapter: unwrap the source element, apply the
    /// key function, wrap the key for the runtime's `K: Eq + Hash` bound.
    fn adapt_key(&mut self, code: &str, src_elem: &CirType, key_ty: &CirType) -> String {
        let arg = self.unwrap("Clone::clone(__x)", src_elem);
        let applied = format!("({code})({arg})");
        let wrapped = self.wrap(&applied, key_ty);
        format!("move |__x| {wrapped}")
    }

    /// `f: Rc<dyn Fn(T) -> R>` → `impl FnOnce(T) -> R`.
    fn adapt_own(&self, code: &str) -> String {
        format!("move |__x| ({code})(__x)")
    }

    /// `f: Rc<dyn Fn((V, V)) -> V>` → `impl Fn(V, V) -> V`.
    fn adapt_own2(&self, code: &str) -> String {
        format!("move |__a, __b| ({code})((__a, __b))")
    }

    /// Source collection → owned iterator expression for the aggregate
    /// combinators (bags expand by multiplicity, §4.8.3).
    fn src_iter(&self, code: &str, ty: &CirType) -> String {
        match ty {
            CirType::Set(_) => format!("({code}).as_slice().iter().cloned()"),
            CirType::Vector(_) => format!("({code}).into_iter()"),
            CirType::Bag(_) => format!("({code}).iter_expanded().cloned()"),
            _ => format!("({code}).into_iter()"),
        }
    }

    fn stdlib_call(&mut self, name: &str, args: &[CirExpr], e: &CirExpr) -> String {
        let a = |i: usize, s: &mut Self| s.expr(&args[i]);
        match name {
            // string
            "contains" => format!("stdlib::contains(&({}), &({}))", a(0, self), a(1, self)),
            "starts_with" => {
                format!("stdlib::starts_with(&({}), &({}))", a(0, self), a(1, self))
            }
            "ends_with" => format!("stdlib::ends_with(&({}), &({}))", a(0, self), a(1, self)),
            "length" => format!("stdlib::str_length(&({}))", a(0, self)),
            "concat" => format!("stdlib::concat(&({}), &({}))", a(0, self), a(1, self)),
            "to_string_int" => format!("stdlib::to_string_int({})", a(0, self)),
            "to_string_float" => format!("stdlib::to_string_float({})", a(0, self)),
            "to_string_date" => format!("stdlib::to_string_date(&({}))", a(0, self)),
            "to_string_bool" => format!("stdlib::to_string_bool({})", a(0, self)),
            "to_string_decimal" => format!("stdlib::to_string_decimal(&({}))", a(0, self)),
            "substring" => format!(
                "stdlib::substring(&({}), {}, {})",
                a(0, self),
                a(1, self),
                a(2, self)
            ),
            "trim" => format!("stdlib::trim(&({}))", a(0, self)),
            "split" => format!("stdlib::split(&({}), &({}))", a(0, self), a(1, self)),
            "join" => format!("stdlib::join(&({}), &({}))", a(0, self), a(1, self)),
            // math
            "abs" => format!("cql_trap(stdlib::abs({}))", a(0, self)),
            "min" => format!("stdlib::min({}, {})", a(0, self), a(1, self)),
            "max" => format!("stdlib::max({}, {})", a(0, self), a(1, self)),
            "floor" => format!("stdlib::floor({})", a(0, self)),
            "ceil" => format!("stdlib::ceil({})", a(0, self)),
            "round" => format!("stdlib::round({})", a(0, self)),
            // decimal
            "decimal_from_string" => {
                let prec = match &e.ty {
                    CirType::Option(t) => match &**t {
                        CirType::Decimal(Some((m, n))) => format!("Some(({m}, {n}))"),
                        _ => "None".to_string(),
                    },
                    _ => "None".to_string(),
                };
                format!("stdlib::decimal_from_string(&({}), {prec})", a(0, self))
            }
            "round_to" => format!("cql_trap(stdlib::round_to(&({}), {}))", a(0, self), a(1, self)),
            // date
            "year" => format!("stdlib::year(&({}))", a(0, self)),
            "month" => format!("stdlib::month(&({}))", a(0, self)),
            "day" => format!("stdlib::day(&({}))", a(0, self)),
            "add_days" => format!("stdlib::add_days(&({}), {})", a(0, self), a(1, self)),
            "days_between" => {
                format!("stdlib::days_between(&({}), &({}))", a(0, self), a(1, self))
            }
            "parse_date" => format!("stdlib::parse_date(&({}))", a(0, self)),
            "day_of_week" => format!("stdlib::day_of_week(&({}))", a(0, self)),
            // vector / iteration
            "fold" => {
                let step = a(2, self);
                let elem = elem_ty(&args[0].ty);
                format!(
                    "stdlib::fold(&({}), {}, {})",
                    a(0, self),
                    a(1, self),
                    self.adapt_elem2(&step, &elem)
                )
            }
            "map" => {
                let f = a(1, self);
                match &args[0].ty {
                    CirType::Vector(t) => {
                        let t = (**t).clone();
                        format!("stdlib::vec_map(&({}), {})", a(0, self), self.adapt_elem(&f, &t))
                    }
                    CirType::Option(_) => {
                        format!("stdlib::option_map({}, {})", a(0, self), self.adapt_own(&f))
                    }
                    _ => {
                        self.err("`map` on a non-vector/non-option value");
                        a(0, self)
                    }
                }
            }
            "filter" => {
                let p = a(1, self);
                let elem = elem_ty(&args[0].ty);
                format!("stdlib::filter(&({}), {})", a(0, self), self.adapt_elem(&p, &elem))
            }
            "append" => format!("stdlib::append(&({}), {})", a(0, self), a(1, self)),
            "to_vector" => {
                let t = match &e.ty {
                    CirType::Vector(t) => (**t).clone(),
                    _ => CirType::Tuple(vec![]),
                };
                let u = self.unwrap("__w", &t);
                format!(
                    "stdlib::to_vector(&({})).into_iter().map(|__w| {u}).collect::<Vec<_>>()",
                    a(0, self)
                )
            }
            "sort_by" => {
                let k = a(1, self);
                let elem = elem_ty(&args[0].ty);
                format!("stdlib::sort_by(&({}), {})", a(0, self), self.adapt_elem(&k, &elem))
            }
            "take" => format!("stdlib::take(&({}), {})", a(0, self), a(1, self)),
            "drop" => format!("stdlib::drop(&({}), {})", a(0, self), a(1, self)),
            "to_set" => {
                let t = match &e.ty {
                    CirType::Set(t) => (**t).clone(),
                    _ => CirType::Tuple(vec![]),
                };
                let x = a(0, self);
                let w = self.wrap("__w", &t);
                format!("stdlib::to_set(({x}).into_iter().map(|__w| {w}).collect::<Vec<_>>())")
            }
            "is_empty" => format!("({}).is_empty()", a(0, self)),
            "concat_vector" => {
                format!("stdlib::concat_vector(&({}), &({}))", a(0, self), a(1, self))
            }
            "scan_left" => {
                let step = a(2, self);
                let elem = elem_ty(&args[0].ty);
                format!(
                    "stdlib::scan_left(&({}), {}, {})",
                    a(0, self),
                    a(1, self),
                    self.adapt_elem2(&step, &elem)
                )
            }
            // set / bag
            "size" => match &args[0].ty {
                CirType::Set(_) => format!("stdlib::size(&({}))", a(0, self)),
                CirType::Bag(_) => format!("({}).total_count() as i64", a(0, self)),
                _ => format!("({}).len() as i64", a(0, self)),
            },
            "the" => {
                let elem = elem_ty(&args[0].ty);
                let inner = format!("Clone::clone(cql_trap(stdlib::the(&({}))))", a(0, self));
                self.unwrap(&inner, &elem)
            }
            "only" => {
                let elem = elem_ty(&args[0].ty);
                let inner =
                    format!("cql_trap(stdlib::only(&({}))).cloned()", a(0, self));
                self.unwrap(&inner, &CirType::Option(Box::new(elem)))
            }
            "union_all" => format!("stdlib::union_all(&({}))", a(0, self)),
            "bag_to_set" => format!("stdlib::bag_to_set(&({}))", a(0, self)),
            "set_to_bag" => format!("stdlib::set_to_bag(&({}))", a(0, self)),
            "copies_in" => {
                let elem = elem_ty(&args[1].ty);
                let x = a(0, self);
                let w = self.wrap(&x, &elem);
                format!("stdlib::copies_in(&({w}), &({}))", a(1, self))
            }
            "bag_union" => format!("stdlib::bag_union(&({}), &({}))", a(0, self), a(1, self)),
            // map
            "map_get" => {
                let (kt, vt) = map_kv(&args[0].ty);
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let kw = self.wrap(&codes[1], &kt);
                let uw = self.unwrap("Clone::clone(__w)", &vt);
                format!(
                    "match stdlib::map_get(&({}), &({kw})) {{ Some(__w) => Some({uw}), None => None }}",
                    codes[0]
                )
            }
            "map_insert" => {
                let (kt, vt) = map_kv(&args[0].ty);
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let kw = self.wrap(&codes[1], &kt);
                let vw = self.wrap(&codes[2], &vt);
                format!("stdlib::map_insert(&({}), {kw}, {vw})", codes[0])
            }
            "map_remove" => {
                let (kt, _) = map_kv(&args[0].ty);
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let kw = self.wrap(&codes[1], &kt);
                format!("stdlib::map_remove(&({}), &({kw}))", codes[0])
            }
            "map_keys" => format!("stdlib::map_keys(&({}))", a(0, self)),
            "map_values" => format!("stdlib::map_values(&({}))", a(0, self)),
            "map_size" => format!("stdlib::map_size(&({}))", a(0, self)),
            "map_from_vector" => {
                let (kt, vt) = match &e.ty {
                    CirType::Map(k, v) => ((**k).clone(), (**v).clone()),
                    _ => (CirType::Tuple(vec![]), CirType::Tuple(vec![])),
                };
                let p = a(0, self);
                let kw = self.wrap("__k", &kt);
                let vw = self.wrap("__v", &vt);
                format!(
                    "stdlib::map_from_vector(({p}).into_iter().map(|(__k, __v)| ({kw}, {vw})).collect::<Vec<_>>())"
                )
            }
            "map_to_vector" => {
                let (kt, vt) = map_kv(&args[0].ty);
                let ku = self.unwrap("__k", &kt);
                let vu = self.unwrap("__v", &vt);
                format!(
                    "stdlib::map_to_vector(&({})).into_iter().map(|(__k, __v)| ({ku}, {vu})).collect::<Vec<_>>()",
                    a(0, self)
                )
            }
            // option
            "and_then" => {
                let f = a(1, self);
                format!("stdlib::and_then({}, {})", a(0, self), self.adapt_own(&f))
            }
            "unwrap_or" => format!("stdlib::unwrap_or({}, {})", a(0, self), a(1, self)),
            "is_some" => format!("stdlib::is_some(&({}))", a(0, self)),
            "is_none" => format!("stdlib::is_none(&({}))", a(0, self)),
            // aggregate combinators (§4.8.3): AggRow rows map to the interned
            // {key, agg} record struct.
            "aggregate" => {
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let src_elem = elem_ty(&args[0].ty);
                let src = self.src_iter(&codes[0], &args[0].ty);
                let key_ty = fun_ret(&args[1].ty);
                let gk = self.adapt_key(&codes[1], &src_elem, &key_ty);
                let val = self.adapt_elem(&codes[2], &src_elem);
                let red = self.adapt_own2(&codes[3]);
                let fin = self.adapt_own(&codes[5]);
                let key_unwrap = self.unwrap("__r.key", &key_ty);
                let rec = self.agg_rec(e);
                format!(
                    "{{ let __agg = stdlib::aggregate({src}, {gk}, {val}, {red}, {}, {fin}); __agg.into_iter().map(|__r| {rec} {{ key: {key_unwrap}, agg: __r.agg }}).collect::<Vec<{rec}>>() }}",
                    codes[4],
                )
            }
            "count_by" => {
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let src_elem = elem_ty(&args[0].ty);
                let src = self.src_iter(&codes[0], &args[0].ty);
                let key_ty = fun_ret(&args[1].ty);
                let gk = self.adapt_key(&codes[1], &src_elem, &key_ty);
                let key_unwrap = self.unwrap("__r.key", &key_ty);
                let rec = self.agg_rec(e);
                format!(
                    "{{ let __agg = stdlib::count_by({src}, {gk}); __agg.into_iter().map(|__r| {rec} {{ key: {key_unwrap}, agg: __r.agg }}).collect::<Vec<{rec}>>() }}"
                )
            }
            "sum_by" | "avg_by" => {
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let src_elem = elem_ty(&args[0].ty);
                let src = self.src_iter(&codes[0], &args[0].ty);
                let key_ty = fun_ret(&args[1].ty);
                let gk = self.adapt_key(&codes[1], &src_elem, &key_ty);
                let val = self.adapt_elem(&codes[2], &src_elem);
                let key_unwrap = self.unwrap("__r.key", &key_ty);
                let rec = self.agg_rec(e);
                let f = if name == "sum_by" {
                    "stdlib::sum_by"
                } else {
                    "stdlib::avg_by"
                };
                format!(
                    "{{ let __agg = {f}({src}, {gk}, {val}); __agg.into_iter().map(|__r| {rec} {{ key: {key_unwrap}, agg: __r.agg }}).collect::<Vec<{rec}>>() }}"
                )
            }
            "min_by" | "max_by" => {
                let codes: Vec<String> = args.iter().map(|x| self.expr(x)).collect();
                let src_elem = elem_ty(&args[0].ty);
                let src = self.src_iter(&codes[0], &args[0].ty);
                let key_ty = fun_ret(&args[1].ty);
                let gk = self.adapt_key(&codes[1], &src_elem, &key_ty);
                let val = self.adapt_elem(&codes[2], &src_elem);
                let key_unwrap = self.unwrap("__r.key", &key_ty);
                let rec = self.agg_rec(e);
                let f = if name == "min_by" {
                    "stdlib::min_by"
                } else {
                    "stdlib::max_by"
                };
                format!(
                    "{{ let __agg = {f}({src}, {gk}, {val}); __agg.into_iter().map(|__r| {rec} {{ key: {key_unwrap}, agg: __r.agg }}).collect::<Vec<{rec}>>() }}"
                )
            }
            _ => {
                self.err(format!("unknown stdlib function `{name}`"));
                "unreachable!()".to_string()
            }
        }
    }

    /// The interned `{key, agg}` record name of an aggregate call's result.
    fn agg_rec(&mut self, e: &CirExpr) -> String {
        match &e.ty {
            CirType::Vector(t) => match &**t {
                CirType::Record(n) => n.clone(),
                _ => {
                    self.err("aggregate result is not a vector of records");
                    "_BadAgg".to_string()
                }
            },
            _ => {
                self.err("aggregate result is not a vector");
                "_BadAgg".to_string()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Float-in-collection wrapping
// ---------------------------------------------------------------------------

/// Element wrapper for floats inside collections (`f64` has no `Eq`/`Hash`).
const CQL_F64_DEF: &str = "#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]\n\
pub struct CqlF64(pub f64);\n\
impl Eq for CqlF64 {}\n\
impl Hash for CqlF64 {\n    fn hash<H: Hasher>(&self, state: &mut H) {\n        self.0.to_bits().hash(state);\n    }\n}\n\
impl CanonOrd for CqlF64 {\n    fn canon_cmp(&self, other: &Self) -> Ordering {\n        self.0.canon_cmp(&other.0)\n    }\n}\n";

impl<'a> Emitter<'a> {
    /// typed value (type `t`) → collection-stored value (`coll_ty(t)`).
    /// Identity for types that need no wrapping.
    fn wrap(&mut self, code: &str, t: &CirType) -> String {
        match t {
            CirType::Float => {
                self.needs_f64.set(true);
                format!("CqlF64({code})")
            }
            CirType::Option(t) if needs_wrap(t) => {
                format!("({code}).map(|__w| {})", self.wrap("__w", t))
            }
            CirType::Vector(t) if needs_wrap(t) => format!(
                "({code}).into_iter().map(|__w| {}).collect::<Vec<_>>()",
                self.wrap("__w", t)
            ),
            CirType::Tuple(ts) if needs_wrap(t) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.wrap(&format!("__w.{i}"), t))
                    .collect();
                format!("{{ let __w = {code}; ({}) }}", elems.join(", "))
            }
            // Sets/bags/maps already store their elements in coll form;
            // records/enums keep their manual-impl shape.
            _ => code.to_string(),
        }
    }

    /// collection-stored value (`coll_ty(t)`) → typed value (type `t`).
    fn unwrap(&mut self, code: &str, t: &CirType) -> String {
        match t {
            CirType::Float => format!("({code}).0"),
            CirType::Option(t) if needs_wrap(t) => {
                format!("({code}).map(|__w| {})", self.unwrap("__w", t))
            }
            CirType::Vector(t) if needs_wrap(t) => format!(
                "({code}).into_iter().map(|__w| {}).collect::<Vec<_>>()",
                self.unwrap("__w", t)
            ),
            CirType::Tuple(ts) if needs_wrap(t) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.unwrap(&format!("__w.{i}"), t))
                    .collect();
                format!("{{ let __w = {code}; ({}) }}", elems.join(", "))
            }
            _ => code.to_string(),
        }
    }

    /// Hash a collection-stored element (`&coll_ty(t)`).
    fn hash_coll_elem(&self, out: &mut String, code: &str, t: &CirType) {
        match t {
            CirType::Float => {
                out.push_str(&format!("        ({code}).0.to_bits().hash(state);\n"))
            }
            CirType::Option(t) if needs_wrap(t) => {
                out.push_str(&format!("        match {code} {{\n"));
                out.push_str("            Some(__h) => { 1u8.hash(state);\n");
                self.hash_coll_elem(out, "__h", t);
                out.push_str("            }\n            None => 0u8.hash(state),\n        }\n");
            }
            CirType::Tuple(ts) if needs_wrap(t) => {
                for (i, t) in ts.iter().enumerate() {
                    self.hash_coll_elem(out, &format!("&({code}).{i}"), t);
                }
            }
            CirType::Vector(t) if needs_wrap(t) => {
                out.push_str(&format!("        ({code}).len().hash(state);\n"));
                out.push_str(&format!("        for __h in ({code}).iter() {{\n"));
                self.hash_coll_elem(out, "__h", t);
                out.push_str("        }\n");
            }
            _ => self.hash_field(out, code, t),
        }
    }

    /// Collection-stored element (`coll_ty(t)`) → runtime `Value`.
    fn to_value_coll(&mut self, code: &str, t: &CirType) -> String {
        match t {
            CirType::Float => format!("Value::Float(({code}).0)"),
            CirType::Option(t) if needs_wrap(t) => format!(
                "match {code} {{ Some(__v) => Value::Option(Some(Box::new({}))), None => Value::Option(None) }}",
                self.to_value_coll("__v", t)
            ),
            CirType::Tuple(ts) if needs_wrap(t) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.to_value_coll(&format!("__t.{i}"), t))
                    .collect();
                format!(
                    "{{ let __t = {code}; Value::Tuple(vec![{}]) }}",
                    elems.join(", ")
                )
            }
            CirType::Vector(t) if needs_wrap(t) => format!(
                "Value::Vector(({code}).into_iter().map(|__v| {}).collect())",
                self.to_value_coll("__v", t)
            ),
            _ => self.to_value(code, t),
        }
    }

    /// Runtime `Value` → collection-stored element (`coll_ty(t)`).
    fn from_value_coll(&mut self, code: &str, t: &CirType) -> String {
        match t {
            CirType::Float => {
                self.needs_f64.set(true);
                format!("CqlF64({})", self.from_value(code, t))
            }
            CirType::Option(t) if needs_wrap(t) => format!(
                "match {code} {{ Value::Option(None) => None, Value::Option(Some(__v)) => Some({}), _ => cql_trap_msg(\"bad value: expected option\") }}",
                self.from_value_coll("&**__v", t)
            ),
            CirType::Tuple(ts) if needs_wrap(t) => {
                let elems: Vec<String> = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| self.from_value_coll(&format!("&__vs[{i}]"), t))
                    .collect();
                format!(
                    "match {code} {{ Value::Tuple(__vs) if __vs.len() == {} => ({}), _ => cql_trap_msg(\"bad value: expected tuple\") }}",
                    ts.len(),
                    elems.join(", ")
                )
            }
            CirType::Vector(t) if needs_wrap(t) => format!(
                "match {code} {{ Value::Vector(__vs) => __vs.iter().map(|__v| {}).collect(), _ => cql_trap_msg(\"bad value: expected vector\") }}",
                self.from_value_coll("__v", t)
            ),
            _ => self.from_value(code, t),
        }
    }
}
