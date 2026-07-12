//! CIR (Codegen IR): the portable intermediate form consumed by all
//! backends (doc/codegen-backend.md §3).
//!
//! CIR is produced by [`lower_to_cir`] from an [`OptimizedModule`]. It is a
//! separate IR — not the AST — containing only constructs mainstream
//! languages can express:
//!
//! - all surface-syntax traces are already gone (desugaring ran earlier);
//! - closures are eliminated by **lambda lifting** (a shared pass run here):
//!   every lambda becomes a top-level [`CirFunDef`] with an explicit
//!   environment parameter; CIR expressions only reference functions via
//!   [`CirExprKind::FunRef`] and construct closures via
//!   [`CirExprKind::MakeClosure`];
//! - effect boundaries are explicit: [`CirExprKind::Read`] /
//!   [`CirExprKind::WriteOp`] only appear inside operator (L1/L2) bodies;
//! - every expression carries its [`CirType`] (isomorphic to the §6.2 ABI
//!   vocabulary), so backends never re-run type inference;
//! - read plans chosen by the optimize pass are materialized in
//!   [`CirExprKind::Read`] nodes.
//!
//! Backends consume CIR only; they never read the AST.

use std::collections::HashMap;

use crate::ast::{BinOpKind, EffectLevel, PatLit, Span, UnOpKind};
use crate::diag::DiagBag;
use crate::optimize::OptimizedModule;
use crate::optimize::ReadPlan;

// ---------------------------------------------------------------------------
// Module-level items
// ---------------------------------------------------------------------------

/// A compiled module: the unit every backend renders.
#[derive(Debug, Clone, PartialEq)]
pub struct CirModule {
    pub name: String,
    /// Tables in declaration order; `CirTable::id` is the stable runtime id.
    pub tables: Vec<CirTable>,
    /// Declared enums (non-generic; generic instantiations are rejected for
    /// the MVP).
    pub enums: Vec<CirEnumDef>,
    /// Structural record types (and table-row structs share this namespace),
    /// interned by canonical field list; fields sorted by name.
    pub records: Vec<CirRecordDef>,
    pub consts: Vec<CirConstDef>,
    /// L0 `function`s plus all lifted lambdas (`env = Some(..)`).
    pub functions: Vec<CirFunDef>,
    /// L1 queries and L2 actions.
    pub operators: Vec<CirOperatorDef>,
    pub invariants: Vec<CirInvariantDef>,
    pub tests: Vec<CirTestDef>,
}

/// A table signature: row/key struct names plus constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct CirTable {
    /// Stable runtime id (1-based, declaration order) used for `TableRef`.
    pub id: u64,
    /// CQL table name.
    pub name: String,
    /// Generated Rust row struct name (fields = all declared columns).
    pub row: String,
    /// Generated Rust key struct name (fields = primary-key columns).
    pub key: String,
    /// All columns in declaration order.
    pub fields: Vec<(String, CirType)>,
    /// Primary-key column names (declaration order).
    pub pk: Vec<String>,
    /// Foreign keys: (referencing columns, referenced table name).
    pub fks: Vec<(Vec<String>, String)>,
    /// Secondary indexes in declaration order: (index name, columns).
    pub indexes: Vec<(String, Vec<String>)>,
}

/// A declared enum mapped to a Rust enum. Payload fields whose type mentions
/// the enum itself are boxed in the generated code (`boxed` flags).
#[derive(Debug, Clone, PartialEq)]
pub struct CirEnumDef {
    /// Rust type name (PascalCase of the CQL name).
    pub name: String,
    pub variants: Vec<CirVariant>,
}

/// A variant of a lowered enum definition ([`CirEnumDef`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CirVariant {
    /// Rust variant name (PascalCase).
    pub name: String,
    pub payload: CirVariantPayload,
}

/// The payload shape of a [`CirVariant`].
#[derive(Debug, Clone, PartialEq)]
pub enum CirVariantPayload {
    /// Unit variant (no payload).
    None,
    /// Positional payload; `boxed[i]` marks self-recursive fields.
    Tuple(Vec<CirType>, Vec<bool>),
    /// Record payload `variant(record { .. })`: lowered to a single
    /// positional payload wrapping the interned record struct.
    Record(String),
}

/// An interned structural record (`{ f: T, ... }`), fields sorted by name.
#[derive(Debug, Clone, PartialEq)]
pub struct CirRecordDef {
    /// Generated Rust struct name (`Rec_<hash>`).
    pub name: String,
    pub fields: Vec<(String, CirType)>,
}

/// `const name: T = value;` — emitted as a zero-argument function.
#[derive(Debug, Clone, PartialEq)]
pub struct CirConstDef {
    pub name: String,
    pub ty: CirType,
    pub value: CirExpr,
}

/// A top-level function: an L0 operator or a lifted lambda.
#[derive(Debug, Clone, PartialEq)]
pub struct CirFunDef {
    /// Rust function name (mangled for generic instantiations / lifted
    /// lambdas).
    pub name: String,
    /// Captured environment for lifted lambdas: (field name, type) pairs.
    /// `None` for plain L0 functions.
    pub env: Option<Vec<(String, CirType)>>,
    pub params: Vec<(String, CirType)>,
    pub ret: CirType,
    /// Let-chains; strict left-to-right evaluation.
    pub body: CirExpr,
}

/// An L1/L2 operator; takes the table state as an implicit first argument.
#[derive(Debug, Clone, PartialEq)]
pub struct CirOperatorDef {
    pub name: String,
    pub level: EffectLevel,
    pub params: Vec<(String, CirType)>,
    pub ret: CirType,
    pub body: CirExpr,
}

/// `invariant Name(table): body` — checked by the runtime on writes.
#[derive(Debug, Clone, PartialEq)]
pub struct CirInvariantDef {
    pub name: String,
    pub table: String,
    pub body: CirExpr,
}

/// A `test` block: fixtures load rows, expects compare values.
#[derive(Debug, Clone, PartialEq)]
pub struct CirTestDef {
    pub name: String,
    /// (table name, rows expression — a vector of row structs).
    pub fixtures: Vec<(String, CirExpr)>,
    /// (lhs, rhs) pairs of an `expect lhs == rhs`.
    pub expects: Vec<(CirExpr, CirExpr)>,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// CIR types: isomorphic to the §6.2 ABI vocabulary (plus `Row`/`WriteOp`
/// which the ABI represents via table resources / the write_op variant).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CirType {
    Bool,
    Int,
    Float,
    /// `decimal(m, n)`; `None` = unbounded decimal.
    Decimal(Option<(u32, u32)>),
    String,
    Date,
    Option(Box<CirType>),
    Vector(Box<CirType>),
    Set(Box<CirType>),
    Bag(Box<CirType>),
    Map(Box<CirType>, Box<CirType>),
    Tuple(Vec<CirType>),
    /// A structural record: name of the interned [`CirRecordDef`].
    Record(String),
    /// A table's row type: name of the table's row struct.
    Row(String),
    /// A declared enum: name of the [`CirEnumDef`].
    Enum(String),
    /// A pure function value. Multi-argument functions take a `Tuple`.
    Fun(Box<CirType>, Box<CirType>),
    /// The type-erased runtime write descriptor (§3.6).
    WriteOp,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// A typed CIR expression.
#[derive(Debug, Clone, PartialEq)]
pub struct CirExpr {
    pub kind: CirExprKind,
    pub ty: CirType,
    pub span: Span,
}

impl CirExpr {
    pub fn new(kind: CirExprKind, ty: CirType, span: Span) -> Self {
        CirExpr { kind, ty, span }
    }
}

/// The kind of a typed CIR expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum CirExprKind {
    Lit(CirLit),
    /// A local binding (parameter, let, pattern binding).
    Var(String),
    /// A captured variable inside a lifted function: field of the env struct.
    EnvGet(String),
    /// A module-level constant used as a value (emitted as a zero-arg call).
    ConstRef(String),
    /// A top-level function (L0 operator or lifted lambda) used as a value.
    FunRef { name: String },
    /// A standard-library function used as a first-class value (single-arg
    /// functions only; the backend emits the reference adapter).
    StdLibRef { name: String },
    /// Move out of a boxed value (`*e`); introduced by pattern compilation
    /// for recursive enum payloads.
    Deref(Box<CirExpr>),
    /// Closure construction: lifted function + captured environment values.
    MakeClosure {
        fun: String,
        env: Vec<(String, CirExpr)>,
    },
    /// A resolved named call (stdlib or module-local operator).
    Call { callee: CirCallee, args: Vec<CirExpr> },
    /// Application of a function-typed value.
    App { func: Box<CirExpr>, args: Vec<CirExpr> },
    /// Let binding with a compiled pattern; refutable patterns trap on
    /// mismatch at runtime.
    Let {
        pat: CirPat,
        value: Box<CirExpr>,
        body: Box<CirExpr>,
    },
    If {
        cond: Box<CirExpr>,
        then_br: Box<CirExpr>,
        else_br: Box<CirExpr>,
    },
    Match {
        scrutinee: Box<CirExpr>,
        arms: Vec<CirArm>,
    },
    RecordLit {
        /// Row struct or interned record struct name.
        def: String,
        fields: Vec<(String, CirExpr)>,
    },
    RecordUpd {
        def: String,
        base: Box<CirExpr>,
        fields: Vec<(String, CirExpr)>,
    },
    Tuple(Vec<CirExpr>),
    Vector(Vec<CirExpr>),
    Set(Vec<CirExpr>),
    Bag(Vec<CirExpr>),
    MapLit(Vec<(CirExpr, CirExpr)>),
    OptionSome(Box<CirExpr>),
    OptionNone,
    BinOp {
        op: BinOpKind,
        lhs: Box<CirExpr>,
        rhs: Box<CirExpr>,
    },
    UnOp { op: UnOpKind, operand: Box<CirExpr> },
    Field { base: Box<CirExpr>, name: String },
    TupleProj { base: Box<CirExpr>, index: u32 },
    EnumConstruct {
        /// Rust enum name.
        def: String,
        /// Rust variant name.
        variant: String,
        args: Vec<CirExpr>,
    },
    /// `e as T` (§2.4 whitelist); `target` is the elaborated target type.
    Cast { target: CirType, expr: Box<CirExpr> },
    /// A table read with the plan chosen by the optimize pass (§5.5).
    /// `key` holds the usable-equality expressions per constrained column
    /// (point-lookup/index plans); `predicate` is the full residual
    /// predicate closure (row -> bool), always evaluated on candidate rows.
    Read {
        table: String,
        plan: ReadPlan,
        key: Vec<(String, CirExpr)>,
        predicate: Box<CirExpr>,
    },
    /// A write-op descriptor construction (L2 only); the atomic application
    /// is performed by the runtime, never inlined by backends.
    WriteOp(CirWriteOp),
}

/// A resolved call target.
#[derive(Debug, Clone, PartialEq)]
pub enum CirCallee {
    /// A module-local operator (mangled name); L1/L2 callees receive the
    /// table state as an implicit first argument.
    Operator { name: String, level: EffectLevel },
    /// A standard-library pure function by its CQL name; the backend maps
    /// `(name, argument types)` to the concrete `cql_runtime::stdlib` item.
    StdLib { name: String },
}

/// A match arm with a compiled pattern. Bindings at boxed (recursive enum
/// payload) positions are rebound through [`CirExprKind::Deref`] lets
/// wrapping `body`.
#[derive(Debug, Clone, PartialEq)]
pub struct CirArm {
    pub pat: CirPat,
    pub body: CirExpr,
}

/// A compiled pattern (decision tree kept shallow: Rust patterns plus a
/// runtime trap for refutable failures).
#[derive(Debug, Clone, PartialEq)]
pub enum CirPat {
    Wildcard,
    Bind(String),
    Lit(PatLit),
    None,
    Some(Box<CirPat>),
    Variant {
        /// Rust enum name.
        def: String,
        /// Rust variant name.
        variant: String,
        args: Vec<CirPat>,
    },
    Tuple(Vec<CirPat>),
    /// Record/row destructuring by field puns: `{ a, b }`.
    Record { def: String, fields: Vec<String> },
}

/// A write-op descriptor (§3.6), mirroring `cql_runtime::WriteOp`.
#[derive(Debug, Clone, PartialEq)]
pub enum CirWriteOp {
    Insert { table: String, row: Box<CirExpr> },
    Update {
        table: String,
        key: Box<CirExpr>,
        transform: Box<CirExpr>,
        /// Static definition id of the transform closure (§3.6 fun_val).
        def_id: u64,
    },
    Delete { table: String, key: Box<CirExpr> },
}

/// Literal values (same shapes as the AST literals).
#[derive(Debug, Clone, PartialEq)]
pub enum CirLit {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Date { year: i32, month: u8, day: u8 },
    Decimal { repr: String, precision: Option<(u32, u32)> },
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

/// Lower an optimized module to CIR (doc/codegen-backend.md §2–3).
///
/// Performs lambda lifting, pattern compilation, generic-operator
/// monomorphization and read-plan materialization. Errors (unsupported
/// constructs for the MVP backend, e.g. generic enum instantiations or
/// cross-module calls to queries/actions) are reported through the returned
/// [`DiagBag`]. See [`lower_to_cir_with_imports`] for multi-module projects.
pub fn lower_to_cir(m: &OptimizedModule) -> Result<CirModule, DiagBag> {
    lower_to_cir_with_imports(m, &[])
}

/// A dependency module's public interface for cross-module calls during
/// CIR lowering (multi-module projects, doc/codegen-backend.md).
///
/// Cross-module references are lowered to qualified Rust paths
/// (`crate::<module>::<item>`): the project driver emits one Rust module
/// file per CQL module, so Rust's module namespaces provide the
/// qualification — no CIR-level renaming/fusion is needed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CirImportModule {
    /// Rust module name of the dependency (sanitized CQL module name).
    pub module: String,
    /// Public operators by CQL name (L0 functions are callable
    /// cross-module; queries/actions are rejected with a diagnostic).
    pub ops: HashMap<String, crate::types::ImportSig>,
    /// Public constants by CQL name.
    pub consts: HashMap<String, Ty>,
}

/// Like [`lower_to_cir`], but with the public interfaces of already-lowered
/// dependencies available for cross-module call lowering.
pub fn lower_to_cir_with_imports(
    m: &OptimizedModule,
    imports: &[CirImportModule],
) -> Result<CirModule, DiagBag> {
    let mut l = Lowerer::new(m);
    l.collect_decls();
    l.collect_imports(imports);
    l.lower_items();
    l.process_worklist();
    l.finish()
}

// ---------------------------------------------------------------------------
// The lowerer
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::collections::VecDeque;

use miette::NamedSource;

use crate::ast::*;
use crate::resolve::{Callee, Resolutions, VarRes};
use crate::types::{Ty, TypedModule};

/// Type-parameter substitution for generic-operator monomorphization.
type Subst = HashMap<String, Ty>;

struct Lowerer<'a> {
    m: &'a OptimizedModule,
    typed: &'a TypedModule,
    resolutions: &'a Resolutions,
    bag: DiagBag,
    src: NamedSource<String>,
    recs: RecordInterner,
    /// Table name → decl (row/key struct names derived by [`pascal`]).
    tables: HashMap<String, &'a TableDecl>,
    /// Table name → secondary indexes in declaration order.
    indexes: HashMap<String, Vec<(String, Vec<String>)>>,
    /// Enum variant name → (enum rust name, payload types, record-payload
    /// struct name if any).
    variants: HashMap<String, VariantInfo>,
    /// Type aliases (expanded on demand for cast targets).
    aliases: HashMap<String, &'a Type>,
    /// Operator decls by name.
    ops: HashMap<String, &'a OperatorDecl>,
    /// Monomorphization worklist: (operator name, substitution).
    worklist: VecDeque<(String, Subst)>,
    done: HashSet<String>,
    functions: Vec<CirFunDef>,
    operators: Vec<CirOperatorDef>,
    tables_out: Vec<CirTable>,
    enums_out: Vec<CirEnumDef>,
    consts: Vec<CirConstDef>,
    invariants: Vec<CirInvariantDef>,
    tests: Vec<CirTestDef>,
    /// Names of module-level constants (fallback classification for
    /// desugarer-synthesized references, which have no side-table entry).
    const_names: HashSet<String>,
    /// CIR types of constants (filled as consts are lowered).
    const_tys: HashMap<String, CirType>,
    /// Imported public operators: CQL name → (dependency's Rust module
    /// name, signature).
    imported_ops: HashMap<String, (String, crate::types::ImportSig)>,
    /// Imported public constants: CQL name → (dependency's Rust module
    /// name, type).
    imported_consts: HashMap<String, (String, Ty)>,
    lift_counter: u32,
    fresh_counter: u32,
}

/// Precomputed payload information for one enum variant.
#[derive(Debug, Clone)]
struct VariantInfo {
    enum_rust: String,
    /// Positional payload types (empty for unit variants).
    tys: Vec<CirType>,
    /// For record payloads: the interned record struct name; the payload is
    /// a single positional field of that type.
    record: Option<String>,
}

/// Lexical scope for expression lowering.
#[derive(Debug, Clone, Default)]
struct Ctx {
    /// Local bindings in scope (innermost last): name → CIR type.
    locals: Vec<HashMap<String, CirType>>,
    /// Captured environment of the enclosing lifted function, if any.
    env: Option<Vec<(String, CirType)>>,
    /// Active type-parameter substitution (generic instantiations).
    subst: Subst,
}

impl Ctx {
    fn lookup(&self, name: &str) -> Option<&CirType> {
        for scope in self.locals.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t);
            }
        }
        self.env
            .as_ref()
            .and_then(|env| env.iter().find(|(n, _)| n == name).map(|(_, t)| t))
    }

    fn is_local(&self, name: &str) -> bool {
        self.locals.iter().rev().any(|s| s.contains_key(name))
    }

    fn bind(&mut self, name: String, ty: CirType) {
        if let Some(scope) = self.locals.last_mut() {
            scope.insert(name, ty);
        }
    }
}

impl<'a> Lowerer<'a> {
    fn new(m: &'a OptimizedModule) -> Self {
        let typed = &m.desugared.typed;
        let name = typed.resolved.module.name.node.clone();
        Lowerer {
            m,
            typed,
            resolutions: &typed.resolved.resolved,
            bag: DiagBag::new(),
            src: NamedSource::new(format!("{name}.cql"), String::new()),
            recs: RecordInterner::default(),
            tables: HashMap::new(),
            indexes: HashMap::new(),
            variants: HashMap::new(),
            aliases: HashMap::new(),
            ops: HashMap::new(),
            worklist: VecDeque::new(),
            done: HashSet::new(),
            functions: Vec::new(),
            operators: Vec::new(),
            tables_out: Vec::new(),
            enums_out: Vec::new(),
            consts: Vec::new(),
            invariants: Vec::new(),
            tests: Vec::new(),
            const_names: HashSet::new(),
            const_tys: HashMap::new(),
            imported_ops: HashMap::new(),
            imported_consts: HashMap::new(),
            lift_counter: 0,
            fresh_counter: 0,
        }
    }

    /// Index the public interfaces of already-lowered dependencies for
    /// cross-module reference lowering.
    fn collect_imports(&mut self, imports: &[CirImportModule]) {
        for im in imports {
            for (name, sig) in &im.ops {
                self.imported_ops
                    .insert(name.clone(), (im.module.clone(), sig.clone()));
            }
            for (name, ty) in &im.consts {
                self.imported_consts
                    .insert(name.clone(), (im.module.clone(), ty.clone()));
            }
        }
    }

    fn err<T>(&mut self, span: Span, msg: impl Into<String>) -> Option<T> {
        self.bag.push_error(crate::diag::CqlError::new(
            self.src.clone(),
            span,
            msg,
            None,
        ));
        None
    }

    // -- declaration collection ---------------------------------------------

    fn collect_decls(&mut self) {
        for item in &self.typed.resolved.module.items {
            match item {
                Item::Table(t) => {
                    self.tables.insert(t.name.node.clone(), t);
                }
                Item::Index(ix) => {
                    self.indexes
                        .entry(ix.table.node.clone())
                        .or_default()
                        .push((
                            ix.name.node.clone(),
                            ix.cols.iter().map(|c| c.node.clone()).collect(),
                        ));
                }
                Item::TypeAlias(a) => {
                    self.aliases.insert(a.name.node.clone(), &a.ty);
                }
                Item::Operator(op) => {
                    self.ops.insert(op.name.node.clone(), op);
                }
                Item::Const(c) => {
                    self.const_names.insert(c.name.node.clone());
                }
                _ => {}
            }
        }
        // Enum variant payloads (non-generic enums only).
        for item in &self.typed.resolved.module.items {
            let Item::Enum(e) = item else { continue };
            if !e.params.is_empty() {
                // Generic enums are lowered on instantiation — unsupported
                // in the MVP; instantiation sites report the error.
                continue;
            }
            let enum_rust = pascal(&e.name.node);
            for v in &e.variants {
                let info = match &v.payload {
                    VariantPayload::None => VariantInfo {
                        enum_rust: enum_rust.clone(),
                        tys: vec![],
                        record: None,
                    },
                    VariantPayload::Tuple(ts) => {
                        let tys: Vec<CirType> = ts
                            .iter()
                            .filter_map(|t| self.surface_ty(t, &Subst::new()))
                            .collect();
                        if tys.len() != ts.len() {
                            continue; // error already reported
                        }
                        VariantInfo {
                            enum_rust: enum_rust.clone(),
                            tys,
                            record: None,
                        }
                    }
                    VariantPayload::Record(fs) => {
                        let fields: Vec<(String, CirType)> = fs
                            .iter()
                            .filter_map(|(n, t)| {
                                self.surface_ty(t, &Subst::new())
                                    .map(|ct| (n.node.clone(), ct))
                            })
                            .collect();
                        if fields.len() != fs.len() {
                            continue;
                        }
                        let rec = self.recs.intern(fields);
                        VariantInfo {
                            enum_rust: enum_rust.clone(),
                            tys: vec![CirType::Record(rec.clone())],
                            record: Some(rec),
                        }
                    }
                };
                self.variants.insert(v.name.node.clone(), info);
            }
        }
    }

    // -- item lowering --------------------------------------------------------

    fn lower_items(&mut self) {
        // Tables (declaration order; 1-based runtime ids).
        let mut tables = Vec::new();
        for item in &self.typed.resolved.module.items {
            let Item::Table(t) = item else { continue };
            let id = tables.len() as u64 + 1;
            let fields: Vec<(String, CirType)> = t
                .fields
                .iter()
                .filter_map(|(n, ty)| {
                    self.surface_ty(ty, &Subst::new())
                        .map(|ct| (n.node.clone(), ct))
                })
                .collect();
            if fields.len() != t.fields.len() {
                continue;
            }
            tables.push(CirTable {
                id,
                name: t.name.node.clone(),
                row: row_struct(&t.name.node),
                key: key_struct(&t.name.node),
                fields,
                pk: t.pk.iter().map(|c| c.node.clone()).collect(),
                fks: t
                    .fks
                    .iter()
                    .map(|fk| {
                        (
                            fk.cols.iter().map(|c| c.node.clone()).collect(),
                            fk.references.node.clone(),
                        )
                    })
                    .collect(),
                indexes: self.indexes.get(&t.name.node).cloned().unwrap_or_default(),
            });
        }
        self.tables_out = tables;

        // Enums (declaration order).
        for item in &self.typed.resolved.module.items {
            let Item::Enum(e) = item else { continue };
            if !e.params.is_empty() {
                continue;
            }
            let mut variants = Vec::new();
            for v in &e.variants {
                let Some(info) = self.variants.get(&v.name.node) else { continue };
                let payload = match &v.payload {
                    VariantPayload::None => CirVariantPayload::None,
                    VariantPayload::Tuple(_) => {
                        let boxed: Vec<bool> = info
                            .tys
                            .iter()
                            .map(|t| mentions_enum(t, &info.enum_rust))
                            .collect();
                        CirVariantPayload::Tuple(info.tys.clone(), boxed)
                    }
                    VariantPayload::Record(_) => {
                        CirVariantPayload::Record(info.record.clone().unwrap())
                    }
                };
                variants.push(CirVariant {
                    name: pascal(&v.name.node),
                    payload,
                });
            }
            self.enums_out.push(CirEnumDef {
                name: pascal(&e.name.node),
                variants,
            });
        }

        // Non-generic operators: lowered with the identity substitution.
        for item in &self.typed.resolved.module.items {
            let Item::Operator(op) = item else { continue };
            if op.type_params.is_empty() {
                self.enqueue(&op.name.node, Subst::new());
            }
        }

        // Consts / invariants / tests.
        let items: Vec<Item> = self.typed.resolved.module.items.clone();
        for item in &items {
            match item {
                Item::Const(c) => {
                    let mut ctx = Ctx::default();
                    if let Some(value) = self.lower_expr(&c.value, &mut ctx) {
                        self.const_tys.insert(c.name.node.clone(), value.ty.clone());
                        self.consts.push(CirConstDef {
                            name: c.name.node.clone(),
                            ty: value.ty.clone(),
                            value,
                        });
                    }
                }
                Item::Invariant(inv) => {
                    let mut ctx = Ctx::default();
                    if let Some(body) = self.lower_expr(&inv.body, &mut ctx) {
                        self.invariants.push(CirInvariantDef {
                            name: inv.name.node.clone(),
                            table: inv.table.node.clone(),
                            body,
                        });
                    }
                }
                Item::Test(t) => {
                    let mut fixtures = Vec::new();
                    let mut expects = Vec::new();
                    for s in &t.stmts {
                        match s {
                            TestStmt::Fixture { table, rows } => {
                                let mut ctx = Ctx::default();
                                if let Some(rows) = self.lower_expr(rows, &mut ctx) {
                                    fixtures.push((table.node.clone(), rows));
                                }
                            }
                            TestStmt::Expect { lhs, rhs } => {
                                let mut ctx = Ctx::default();
                                if let (Some(l), Some(r)) =
                                    (self.lower_expr(lhs, &mut ctx), self.lower_expr(rhs, &mut ctx))
                                {
                                    expects.push((l, r));
                                }
                            }
                        }
                    }
                    self.tests.push(CirTestDef {
                        name: t.name.node.clone(),
                        fixtures,
                        expects,
                    });
                }
                _ => {}
            }
        }
    }

    /// Enqueue one operator instantiation for lowering.
    fn enqueue(&mut self, name: &str, subst: Subst) {
        let mangled = mangle(name, &subst);
        if self.done.contains(&mangled) {
            return;
        }
        self.done.insert(mangled);
        self.worklist.push_back((name.to_string(), subst));
    }

    fn process_worklist(&mut self) {
        while let Some((name, subst)) = self.worklist.pop_front() {
            let Some(op) = self.ops.get(&name).copied() else { continue };
            let Some(body_ast) = &op.body else {
                self.err::<()>(
                    op.name.span,
                    format!(
                        "external operator `{}` has no body; codegen MVP requires definitions",
                        name
                    ),
                );
                continue;
            };
            let mangled = mangle(&name, &subst);
            // Signature from the elaborated side table.
            let Some(sig) = self.typed.operator_sigs.get(&name).cloned() else {
                continue;
            };
            let params: Vec<(String, CirType)> = sig
                .params
                .iter()
                .filter_map(|(n, t)| {
                    self.cir_ty(&subst_ty(t, &subst), &subst)
                        .map(|ct| (n.clone(), ct))
                })
                .collect();
            let Some(ret) = self.cir_ty(&subst_ty(&sig.ret, &subst), &subst) else {
                continue;
            };
            if params.len() != sig.params.len() {
                continue;
            }
            let mut ctx = Ctx {
                locals: vec![HashMap::new()],
                env: None,
                subst: subst.clone(),
            };
            for (n, ct) in &params {
                ctx.bind(n.clone(), ct.clone());
            }
            if let Some(body) = self.lower_expr_ex(body_ast, &mut ctx, Some(&ret)) {
                match op.level {
                    EffectLevel::Function => self.functions.push(CirFunDef {
                        name: mangled,
                        env: None,
                        params,
                        ret,
                        body,
                    }),
                    level => self.operators.push(CirOperatorDef {
                        name: mangled,
                        level,
                        params,
                        ret,
                        body,
                    }),
                }
            }
        }
    }

    fn finish(self) -> Result<CirModule, DiagBag> {
        let module = CirModule {
            name: self.typed.resolved.module.name.node.clone(),
            tables: self.tables_out,
            enums: self.enums_out,
            records: self.recs.defs(),
            consts: self.consts,
            functions: self.functions,
            operators: self.operators,
            invariants: self.invariants,
            tests: self.tests,
        };
        self.bag.into_result(module)
    }

    // -- type conversion ------------------------------------------------------

    /// Convert an elaborated [`Ty`] to a [`CirType`].
    fn cir_ty(&mut self, ty: &Ty, subst: &Subst) -> Option<CirType> {
        let ct = match ty {
            Ty::Bool => CirType::Bool,
            Ty::Int => CirType::Int,
            Ty::Float => CirType::Float,
            Ty::Decimal(p) => CirType::Decimal(*p),
            Ty::String => CirType::String,
            Ty::Date => CirType::Date,
            Ty::Option(t) => CirType::Option(Box::new(self.cir_ty(t, subst)?)),
            Ty::Vector(t) => CirType::Vector(Box::new(self.cir_ty(t, subst)?)),
            Ty::Set(t) => CirType::Set(Box::new(self.cir_ty(t, subst)?)),
            Ty::Bag(t) => CirType::Bag(Box::new(self.cir_ty(t, subst)?)),
            Ty::Map(k, v) => CirType::Map(
                Box::new(self.cir_ty(k, subst)?),
                Box::new(self.cir_ty(v, subst)?),
            ),
            Ty::Tuple(ts) => {
                let mut out = Vec::new();
                for t in ts {
                    out.push(self.cir_ty(t, subst)?);
                }
                CirType::Tuple(out)
            }
            Ty::Record(fs) => {
                let mut out = Vec::new();
                for (n, t) in fs {
                    out.push((n.clone(), self.cir_ty(t, subst)?));
                }
                // §2.2 row/record equivalence: a structural record identical
                // to a table's row shape must lower to the row struct, so
                // values of the two type-checker views stay type-compatible
                // in the generated code.
                match self.row_for_record(&out) {
                    Some(row) => CirType::Row(row),
                    None => CirType::Record(self.recs.intern(out)),
                }
            }
            Ty::Row(t) => CirType::Row(row_struct(t)),
            Ty::Enum { name, args } => {
                if !args.is_empty() {
                    return self.err(
                        Span::new_dummy(),
                        format!(
                            "generic enum `{}` instantiation is not supported by codegen MVP",
                            name
                        ),
                    );
                }
                CirType::Enum(pascal(name))
            }
            Ty::Fun(a, b) => CirType::Fun(
                Box::new(self.cir_ty(a, subst)?),
                Box::new(self.cir_ty(b, subst)?),
            ),
            Ty::WriteOp => CirType::WriteOp,
            Ty::Var(n) => match subst.get(n) {
                Some(t) => return self.cir_ty(t, subst),
                None => {
                    return self.err(
                        Span::new_dummy(),
                        format!("unsubstituted type parameter `{n}` during CIR lowering"),
                    )
                }
            },
            Ty::Error => {
                return self.err(
                    Span::new_dummy(),
                    "error type reached CIR lowering (frontend should have rejected)",
                )
            }
        };
        Some(ct)
    }

    /// If `fields` is exactly the field list of a declared table (any
    /// order), return that table's row struct name.
    fn row_for_record(&self, fields: &[(String, CirType)]) -> Option<String> {
        let mut sorted: Vec<(String, CirType)> = fields.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        self.tables_out.iter().find_map(|t| {
            let mut tf = t.fields.clone();
            tf.sort_by(|a, b| a.0.cmp(&b.0));
            if tf == sorted {
                Some(t.row.clone())
            } else {
                None
            }
        })
    }

    /// Convert a surface type annotation to an elaborated [`Ty`] (used for
    /// declaration field types and cast targets, where no inference ran).
    fn surface_ty(&mut self, t: &Type, subst: &Subst) -> Option<CirType> {
        let ty = self.surface_ty_to_ty(t, 0)?;
        self.cir_ty(&ty, subst)
    }

    fn surface_ty_to_ty(&mut self, t: &Type, depth: u32) -> Option<Ty> {
        if depth > 32 {
            return self.err(t.span, "type alias expansion too deep (cyclic alias?)");
        }
        let ty = match &t.kind {
            TypeKind::Bool => Ty::Bool,
            TypeKind::Int => Ty::Int,
            TypeKind::Float => Ty::Float,
            TypeKind::Decimal(p) => Ty::Decimal(*p),
            TypeKind::String => Ty::String,
            TypeKind::Date => Ty::Date,
            TypeKind::Option(i) => Ty::Option(Box::new(self.surface_ty_to_ty(i, depth)?)),
            TypeKind::Vector(i) => Ty::Vector(Box::new(self.surface_ty_to_ty(i, depth)?)),
            TypeKind::Set(i) => Ty::Set(Box::new(self.surface_ty_to_ty(i, depth)?)),
            TypeKind::Bag(i) => Ty::Bag(Box::new(self.surface_ty_to_ty(i, depth)?)),
            TypeKind::Map(k, v) => Ty::Map(
                Box::new(self.surface_ty_to_ty(k, depth)?),
                Box::new(self.surface_ty_to_ty(v, depth)?),
            ),
            TypeKind::Tuple(ts) => {
                let mut out = Vec::new();
                for x in ts {
                    out.push(self.surface_ty_to_ty(x, depth)?);
                }
                Ty::Tuple(out)
            }
            TypeKind::Fun(a, b) => Ty::Fun(
                Box::new(self.surface_ty_to_ty(a, depth)?),
                Box::new(self.surface_ty_to_ty(b, depth)?),
            ),
            TypeKind::Record(fs) => {
                let mut out = Vec::new();
                for (n, x) in fs {
                    out.push((n.node.clone(), self.surface_ty_to_ty(x, depth)?));
                }
                Ty::Record(out)
            }
            TypeKind::Named { name, args } => {
                if name.node == "write_op" {
                    Ty::WriteOp
                } else if let Some(alias) = self.aliases.get(&name.node).copied() {
                    if !args.is_empty() {
                        return self.err(
                            t.span,
                            "generic type aliases are not supported by codegen MVP",
                        );
                    }
                    return self.surface_ty_to_ty(alias, depth + 1);
                } else if self.tables.contains_key(&name.node) {
                    Ty::Row(name.node.clone())
                } else {
                    // Enum name (non-generic at this point).
                    if !args.is_empty() {
                        return self.err(
                            t.span,
                            format!(
                                "generic type `{}` instantiation is not supported by codegen MVP",
                                name.node
                            ),
                        );
                    }
                    Ty::Enum {
                        name: name.node.clone(),
                        args: vec![],
                    }
                }
            }
            TypeKind::Key(table) => {
                let Some(decl) = self.tables.get(&table.node).copied() else {
                    return self.err(t.span, format!("unknown table `{}`", table.node));
                };
                let mut comps = Vec::new();
                for pk in &decl.pk {
                    let Some((_, fty)) = decl.fields.iter().find(|(n, _)| n.node == pk.node)
                    else {
                        continue;
                    };
                    comps.push(self.surface_ty_to_ty(fty, depth)?);
                }
                if comps.len() == 1 {
                    comps.pop().unwrap()
                } else {
                    Ty::Tuple(comps)
                }
            }
            TypeKind::Value(table) => Ty::Row(table.node.clone()),
            TypeKind::Table(..) => {
                return self.err(t.span, "`table<...>` types are not codegen-able values")
            }
        };
        Some(ty)
    }

    // -- expression lowering ----------------------------------------------------

    fn lower_expr(&mut self, e: &Expr, ctx: &mut Ctx) -> Option<CirExpr> {
        self.lower_expr_ex(e, ctx, None)
    }

    /// Lower an expression, deriving its CIR type bottom-up.
    ///
    /// The type side table is only a *hint*: the desugarer synthesizes core
    /// nodes reusing the original expression's span, so a synthesized node's
    /// side-table entry (if any) holds the replaced expression's type, not
    /// its own. `expected` carries the type demanded by the enclosing
    /// context (used for lambdas and empty literals).
    fn lower_expr_ex(
        &mut self,
        e: &Expr,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<CirExpr> {
        let span = e.span;
        let out = match &e.kind {
            ExprKind::Lit(l) => {
                let (kind, ty) = match l {
                    Literal::Bool(b) => (CirLit::Bool(*b), CirType::Bool),
                    Literal::Int(i) => (CirLit::Int(*i), CirType::Int),
                    Literal::Float(f) => (CirLit::Float(*f), CirType::Float),
                    Literal::Str(s) => (CirLit::Str(s.clone()), CirType::String),
                    Literal::Date { year, month, day } => (
                        CirLit::Date {
                            year: *year,
                            month: *month,
                            day: *day,
                        },
                        CirType::Date,
                    ),
                    Literal::Decimal { repr, precision } => (
                        CirLit::Decimal {
                            repr: repr.clone(),
                            precision: *precision,
                        },
                        CirType::Decimal(*precision),
                    ),
                };
                CirExpr::new(CirExprKind::Lit(kind), ty, span)
            }
            ExprKind::Var(name) => return self.lower_var(name, span, ctx, expected),
            ExprKind::Let { pat, value, body } => {
                let value = self.lower_expr(value, ctx)?;
                let vty = value.ty.clone();
                let mut wrappers = Vec::new();
                let cp = self.lower_pat(pat, &vty, &mut wrappers, span)?;
                ctx.locals.push(HashMap::new());
                self.bind_pat_names(pat, &vty, ctx, span)?;
                let mut body = self.lower_expr_ex(body, ctx, expected)?;
                ctx.locals.pop();
                for (wp, wv) in wrappers.into_iter().rev() {
                    let wty = body.ty.clone();
                    let wspan = body.span;
                    body = CirExpr::new(
                        CirExprKind::Let {
                            pat: wp,
                            value: Box::new(wv),
                            body: Box::new(body),
                        },
                        wty,
                        wspan,
                    );
                }
                let ty = body.ty.clone();
                CirExpr::new(
                    CirExprKind::Let {
                        pat: cp,
                        value: Box::new(value),
                        body: Box::new(body),
                    },
                    ty,
                    span,
                )
            }
            ExprKind::Lambda(_) => return self.lift_lambda(e, ctx, expected),
            ExprKind::App { func, args } => {
                let func = self.lower_expr(func, ctx)?;
                let (pty, rty) = match &func.ty {
                    CirType::Fun(p, r) => ((**p).clone(), (**r).clone()),
                    _ => return self.err(span, "application of a non-function value"),
                };
                let arg_expects = split_tuple(&pty, args.len());
                let mut out = Vec::new();
                for (a, ae) in args.iter().zip(arg_expects) {
                    out.push(self.lower_expr_ex(&a.value, ctx, ae.as_ref())?);
                }
                CirExpr::new(
                    CirExprKind::App {
                        func: Box::new(func),
                        args: out,
                    },
                    rty,
                    span,
                )
            }
            ExprKind::Call(call) => return self.lower_call(call, span, ctx, expected),
            ExprKind::Match { scrutinee, arms } => {
                return self.lower_match(scrutinee, arms, span, ctx, expected)
            }
            ExprKind::If {
                cond,
                then_br,
                else_br,
            } => {
                let cond = self.lower_expr(cond, ctx)?;
                let then_br = self.lower_expr_ex(then_br, ctx, expected)?;
                let ty = then_br.ty.clone();
                let else_br = self.lower_expr_ex(else_br, ctx, expected)?;
                CirExpr::new(
                    CirExprKind::If {
                        cond: Box::new(cond),
                        then_br: Box::new(then_br),
                        else_br: Box::new(else_br),
                    },
                    ty,
                    span,
                )
            }
            ExprKind::RecordLit { fields } => {
                let hint = self.hint_ty(span, ctx);
                let mut out = Vec::new();
                for f in fields {
                    out.push((f.name.node.clone(), self.lower_expr(&f.value, ctx)?));
                }
                let def = match hint {
                    Some(CirType::Row(r)) => r,
                    Some(CirType::Record(r)) => r,
                    _ => self.recs.intern(
                        out.iter()
                            .map(|(n, x)| (n.clone(), x.ty.clone()))
                            .collect(),
                    ),
                };
                let ty = if self
                    .tables_out
                    .iter()
                    .any(|t| t.row == def)
                {
                    CirType::Row(def.clone())
                } else {
                    CirType::Record(def.clone())
                };
                CirExpr::new(CirExprKind::RecordLit { def, fields: out }, ty, span)
            }
            ExprKind::RecordUpd { base, fields } => {
                let base = self.lower_expr(base, ctx)?;
                let def = match &base.ty {
                    CirType::Row(r) => r.clone(),
                    CirType::Record(r) => r.clone(),
                    _ => return self.err(span, "record update with non-record base"),
                };
                let mut out = Vec::new();
                for f in fields {
                    out.push((f.name.node.clone(), self.lower_expr(&f.value, ctx)?));
                }
                let ty = base.ty.clone();
                CirExpr::new(
                    CirExprKind::RecordUpd {
                        def,
                        base: Box::new(base),
                        fields: out,
                    },
                    ty,
                    span,
                )
            }
            ExprKind::Tuple(xs) => {
                let mut out = Vec::new();
                let mut tys = Vec::new();
                for x in xs {
                    let x = self.lower_expr(x, ctx)?;
                    tys.push(x.ty.clone());
                    out.push(x);
                }
                CirExpr::new(CirExprKind::Tuple(out), CirType::Tuple(tys), span)
            }
            ExprKind::Vector(xs) => {
                let (out, elem) = self.lower_homogeneous(xs, span, ctx, expected)?;
                CirExpr::new(CirExprKind::Vector(out), CirType::Vector(Box::new(elem)), span)
            }
            ExprKind::SetLiteral(xs) => {
                let (out, elem) = self.lower_homogeneous(xs, span, ctx, expected)?;
                CirExpr::new(CirExprKind::Set(out), CirType::Set(Box::new(elem)), span)
            }
            ExprKind::BagLiteral(xs) => {
                let (out, elem) = self.lower_homogeneous(xs, span, ctx, expected)?;
                CirExpr::new(CirExprKind::Bag(out), CirType::Bag(Box::new(elem)), span)
            }
            ExprKind::MapLit(kvs) => {
                let mut out = Vec::new();
                let mut kv = None;
                for (k, v) in kvs {
                    let k = self.lower_expr(k, ctx)?;
                    let v = self.lower_expr(v, ctx)?;
                    kv = Some((k.ty.clone(), v.ty.clone()));
                    out.push((k, v));
                }
                let (kt, vt) = match (kv, expected, self.hint_ty(span, ctx)) {
                    (Some(kv), _, _) => kv,
                    (None, Some(CirType::Map(k, v)), _) => ((**k).clone(), (**v).clone()),
                    (None, _, Some(CirType::Map(k, v))) => (*k, *v),
                    _ => return self.err(span, "cannot infer the type of an empty map literal"),
                };
                CirExpr::new(CirExprKind::MapLit(out), CirType::Map(Box::new(kt), Box::new(vt)), span)
            }
            ExprKind::OptionSome(x) => {
                let inner_expected = match expected {
                    Some(CirType::Option(t)) => Some((**t).clone()),
                    _ => None,
                };
                let x = self.lower_expr_ex(x, ctx, inner_expected.as_ref())?;
                let ty = CirType::Option(Box::new(x.ty.clone()));
                CirExpr::new(CirExprKind::OptionSome(Box::new(x)), ty, span)
            }
            ExprKind::OptionNone => {
                let ty = match (expected, self.hint_ty(span, ctx)) {
                    (Some(t @ CirType::Option(_)), _) => t.clone(),
                    (_, Some(t @ CirType::Option(_))) => t,
                    _ => return self.err(span, "cannot infer the type of `none`"),
                };
                CirExpr::new(CirExprKind::OptionNone, ty, span)
            }
            ExprKind::Cast { expr, ty: target } => {
                let target = self.surface_ty(target, &ctx.subst)?;
                let expr = self.lower_expr(expr, ctx)?;
                CirExpr::new(
                    CirExprKind::Cast {
                        target: target.clone(),
                        expr: Box::new(expr),
                    },
                    target,
                    span,
                )
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let lhs = self.lower_expr(lhs, ctx)?;
                let rhs = self.lower_expr(rhs, ctx)?;
                let ty = match op {
                    BinOpKind::Eq
                    | BinOpKind::Ne
                    | BinOpKind::Lt
                    | BinOpKind::Gt
                    | BinOpKind::Le
                    | BinOpKind::Ge
                    | BinOpKind::And
                    | BinOpKind::Or
                    | BinOpKind::Impl
                    | BinOpKind::In
                    | BinOpKind::SubsetEq => CirType::Bool,
                    BinOpKind::Add
                    | BinOpKind::Sub
                    | BinOpKind::Mul
                    | BinOpKind::Div
                    | BinOpKind::Mod
                    | BinOpKind::Cup
                    | BinOpKind::Cap
                    | BinOpKind::Diff => lhs.ty.clone(),
                    BinOpKind::Cartesian => {
                        let a = elem_ty(&lhs.ty);
                        let b = elem_ty(&rhs.ty);
                        match (a, b) {
                            (Some(a), Some(b)) => {
                                CirType::Set(Box::new(CirType::Tuple(vec![a, b])))
                            }
                            _ => return self.err(span, "cartesian product of non-sets"),
                        }
                    }
                };
                CirExpr::new(
                    CirExprKind::BinOp {
                        op: *op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty,
                    span,
                )
            }
            ExprKind::UnOp { op, operand } => {
                let operand = self.lower_expr(operand, ctx)?;
                let ty = match op {
                    UnOpKind::Not => CirType::Bool,
                    UnOpKind::Neg => operand.ty.clone(),
                };
                CirExpr::new(
                    CirExprKind::UnOp {
                        op: *op,
                        operand: Box::new(operand),
                    },
                    ty,
                    span,
                )
            }
            ExprKind::Field { base, name } => {
                let base = self.lower_expr(base, ctx)?;
                let fty = self.field_ty(&base.ty, &name.node, span)?;
                CirExpr::new(
                    CirExprKind::Field {
                        base: Box::new(base),
                        name: sanitize(&name.node),
                    },
                    fty,
                    span,
                )
            }
            ExprKind::TupleProj { base, index } => {
                let base = self.lower_expr(base, ctx)?;
                let ty = match &base.ty {
                    CirType::Tuple(ts) => match ts.get(*index as usize) {
                        Some(t) => t.clone(),
                        None => return self.err(span, "tuple projection out of range"),
                    },
                    _ => return self.err(span, "tuple projection of non-tuple"),
                };
                CirExpr::new(
                    CirExprKind::TupleProj {
                        base: Box::new(base),
                        index: *index,
                    },
                    ty,
                    span,
                )
            }
            ExprKind::EnumConstruct { name, args } => {
                let def = match (self.hint_ty(span, ctx), expected) {
                    (Some(CirType::Enum(d)), _) => d,
                    (_, Some(CirType::Enum(d))) => d.clone(),
                    _ => {
                        return self
                            .err(span, "cannot determine the enum type of this construction")
                    }
                };
                let mut out = Vec::new();
                for x in args {
                    out.push(self.lower_expr(x, ctx)?);
                }
                CirExpr::new(
                    CirExprKind::EnumConstruct {
                        def: def.clone(),
                        variant: pascal(&name.node),
                        args: out,
                    },
                    CirType::Enum(def),
                    span,
                )
            }
            ExprKind::ReadPrim { table, predicate } => {
                return self.lower_read(table, predicate, span, ctx)
            }
            ExprKind::WriteCon(w) => CirExpr::new(
                CirExprKind::WriteOp(match w {
                    WriteCon::Insert { table, row } => {
                        let row_ty = CirType::Row(row_struct(&table.node));
                        CirWriteOp::Insert {
                            table: table.node.clone(),
                            row: Box::new(self.lower_expr_ex(row, ctx, Some(&row_ty))?),
                        }
                    }
                    WriteCon::Update {
                        table,
                        key,
                        transform,
                    } => {
                        let def_id = self.fresh();
                        let tr_ty = CirType::Fun(
                            Box::new(CirType::Row(row_struct(&table.node))),
                            Box::new(CirType::Row(row_struct(&table.node))),
                        );
                        CirWriteOp::Update {
                            table: table.node.clone(),
                            key: Box::new(self.lower_expr(key, ctx)?),
                            transform: Box::new(self.lower_expr_ex(transform, ctx, Some(&tr_ty))?),
                            def_id,
                        }
                    }
                    WriteCon::Delete { table, key } => CirWriteOp::Delete {
                        table: table.node.clone(),
                        key: Box::new(self.lower_expr(key, ctx)?),
                    },
                }),
                CirType::WriteOp,
                span,
            ),
            ExprKind::Primed(_) => {
                return self.err(span, "`'` (prime) is only meaningful inside properties")
            }
            surface => {
                return self.err(
                    span,
                    format!(
                        "surface-only node {:?} survived desugaring; cannot lower to CIR",
                        surface
                    ),
                )
            }
        };
        Some(out)
    }

    /// Lower a homogeneous literal's elements, deriving the element type
    /// (from the first element, or from context for empty literals).
    fn lower_homogeneous(
        &mut self,
        xs: &[Expr],
        span: Span,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<(Vec<CirExpr>, CirType)> {
        let elem_expected = match expected {
            Some(CirType::Vector(t)) | Some(CirType::Set(t)) | Some(CirType::Bag(t)) => {
                Some((**t).clone())
            }
            _ => None,
        };
        let mut out = Vec::new();
        for x in xs {
            out.push(self.lower_expr_ex(x, ctx, elem_expected.as_ref())?);
        }
        let elem = match out.first() {
            Some(x) => x.ty.clone(),
            None => match (elem_expected, self.hint_ty(span, ctx)) {
                (Some(t), _) => t,
                (None, Some(CirType::Vector(t)))
                | (None, Some(CirType::Set(t)))
                | (None, Some(CirType::Bag(t))) => *t,
                _ => return self.err(Span::new_dummy(), "cannot infer the type of an empty literal"),
            },
        };
        Some((out, elem))
    }

    /// Field type of a record/row type.
    fn field_ty(&mut self, base: &CirType, name: &str, span: Span) -> Option<CirType> {
        match base {
            CirType::Record(r) => {
                let fields = self.recs.fields_of(r).map(|fs| fs.to_vec());
                fields
                    .and_then(|fs| fs.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()))
                    .or_else(|| self.err(span, format!("record has no field `{name}`")))
            }
            CirType::Row(r) => {
                let fields: Vec<(String, CirType)> = self
                    .tables_out
                    .iter()
                    .find(|t| &t.row == r)
                    .map(|t| t.fields.clone())
                    .unwrap_or_default();
                fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t.clone())
                    .or_else(|| self.err(span, format!("row `{r}` has no field `{name}`")))
            }
            _ => self.err(span, "field access on a non-record value"),
        }
    }

    /// Quiet side-table type lookup (no diagnostics; skips error types).
    fn hint_ty(&mut self, span: Span, ctx: &Ctx) -> Option<CirType> {
        let ty = self.typed.expr_tys.get(&span)?.clone();
        let ty = subst_ty(&ty, &ctx.subst);
        if contains_error(&ty) {
            return None;
        }
        self.cir_ty(&ty, &ctx.subst)
    }

    fn fresh(&mut self) -> u64 {
        self.fresh_counter += 1;
        self.fresh_counter as u64
    }
}

/// Interner for structural record types: identical canonical field lists
/// share one generated struct name.
#[derive(Debug, Default)]
pub(crate) struct RecordInterner {
    by_fields: HashMap<Vec<(String, CirType)>, String>,
    defs: Vec<CirRecordDef>,
}

impl RecordInterner {
    /// Intern a structural record; fields are sorted by name (canonical
    /// order, consistent with the erased `Value::Record` encoding).
    pub(crate) fn intern(&mut self, mut fields: Vec<(String, CirType)>) -> String {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(name) = self.by_fields.get(&fields) {
            return name.clone();
        }
        let name = format!("Rec_{:x}", fnv1a(&format!("{:?}", fields)));
        self.by_fields.insert(fields.clone(), name.clone());
        self.defs.push(CirRecordDef {
            name: name.clone(),
            fields,
        });
        name
    }

    pub(crate) fn defs(self) -> Vec<CirRecordDef> {
        self.defs
    }

    /// Field list of an interned record (for pattern binding).
    pub(crate) fn fields_of(&self, name: &str) -> Option<&[(String, CirType)]> {
        self.defs
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.fields.as_slice())
    }
}

/// Deterministic FNV-1a hash for generating stable record struct names.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Lowering: variables, calls, lambdas, match, reads, patterns
// ---------------------------------------------------------------------------

impl<'a> Lowerer<'a> {
    fn lower_var(
        &mut self,
        name: &Ident,
        span: Span,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<CirExpr> {
        let res = self.resolutions.vars.get(&name.span).cloned();
        let kind = match res {
            Some(VarRes::Local) => {
                if ctx.is_local(&name.node) {
                    CirExprKind::Var(sanitize(&name.node))
                } else if ctx.lookup(&name.node).is_some() {
                    CirExprKind::EnvGet(sanitize(&name.node))
                } else {
                    return self.err(span, format!("unbound local `{}` in CIR lowering", name.node));
                }
            }
            Some(VarRes::Const) => {
                if let Some((rust_mod, _)) = self.imported_consts.get(&name.node) {
                    CirExprKind::ConstRef(format!("crate::{}::{}", rust_mod, name.node))
                } else {
                    CirExprKind::ConstRef(name.node.clone())
                }
            }
            Some(VarRes::Function) => {
                if self.ops.contains_key(&name.node) {
                    CirExprKind::FunRef {
                        name: sanitize(&name.node),
                    }
                } else if let Some((rust_mod, isig)) = self.imported_ops.get(&name.node).cloned() {
                    // Imported function used as a first-class value.
                    if !isig.type_params.is_empty() || isig.sig.params.len() > 1 {
                        return self.err(
                            span,
                            format!(
                                "first-class use of imported function `{}` is not supported yet; call it directly",
                                name.node
                            ),
                        );
                    }
                    CirExprKind::FunRef {
                        name: format!("crate::{}::{}", rust_mod, sanitize(&name.node)),
                    }
                } else {
                    CirExprKind::FunRef {
                        name: sanitize(&name.node),
                    }
                }
            }
            Some(VarRes::StdLibFn) => CirExprKind::StdLibRef {
                name: name.node.clone(),
            },
            Some(VarRes::TableSugar) => {
                return self.err(span, "table-name sugar survived desugaring")
            }
            None => {
                // Desugarer-synthesized reference: classify by name.
                if ctx.is_local(&name.node) {
                    CirExprKind::Var(sanitize(&name.node))
                } else if ctx.lookup(&name.node).is_some() {
                    CirExprKind::EnvGet(sanitize(&name.node))
                } else if self.const_names.contains(&name.node) {
                    CirExprKind::ConstRef(name.node.clone())
                } else if let Some((rust_mod, _)) = self.imported_consts.get(&name.node) {
                    CirExprKind::ConstRef(format!("crate::{}::{}", rust_mod, name.node))
                } else if self.ops.contains_key(&name.node) {
                    CirExprKind::FunRef {
                        name: sanitize(&name.node),
                    }
                } else if crate::resolve::stdlib_signature(&name.node).is_some() {
                    CirExprKind::StdLibRef {
                        name: name.node.clone(),
                    }
                } else {
                    return self.err(span, format!("unresolved variable `{}`", name.node));
                }
            }
        };
        let ty = match &kind {
            CirExprKind::Var(n) | CirExprKind::EnvGet(n) => ctx.lookup(n).cloned()?,
            CirExprKind::ConstRef(n) => {
                // Local name (strip the `crate::<mod>::` qualification of
                // cross-module references).
                let plain = n.rsplit("::").next().unwrap_or(n).to_string();
                match self.const_tys.get(&plain) {
                    Some(t) => t.clone(),
                    None => self.hint_ty(span, ctx)?,
                }
            }
            CirExprKind::FunRef { .. } => {
                let sig = self.typed.operator_sigs.get(&name.node).cloned();
                match sig {
                    Some(sig) => {
                        let param = if sig.params.len() == 1 {
                            self.cir_ty(&sig.params[0].1, &ctx.subst)?
                        } else {
                            let mut ts = Vec::new();
                            for (_, t) in &sig.params {
                                ts.push(self.cir_ty(t, &ctx.subst)?);
                            }
                            CirType::Tuple(ts)
                        };
                        let ret = self.cir_ty(&sig.ret, &ctx.subst)?;
                        CirType::Fun(Box::new(param), Box::new(ret))
                    }
                    None => self.hint_ty(span, ctx)?,
                }
            }
            CirExprKind::StdLibRef { .. } => match (expected, self.hint_ty(span, ctx)) {
                (Some(t), _) => t.clone(),
                (_, Some(t)) => t,
                _ => {
                    return self.err(
                        span,
                        "cannot infer the type of a first-class stdlib reference",
                    )
                }
            },
            _ => unreachable!(),
        };
        Some(CirExpr::new(kind, ty, span))
    }

    /// Lower a resolved named call (stdlib / operator / local or const fn
    /// value), reordering named arguments to declaration order and deriving
    /// the result type from the arguments.
    fn lower_call(
        &mut self,
        call: &Call,
        span: Span,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<CirExpr> {
        let callee = self.resolutions.callee.get(&call.name.span).cloned().or_else(|| {
            // Desugarer-synthesized call: classify by name.
            if let Some(op) = self.ops.get(&call.name.node) {
                Some(Callee::Operator {
                    name: call.name.node.clone(),
                    level: op.level,
                    module_local: true,
                })
            } else if crate::resolve::stdlib_signature(&call.name.node).is_some() {
                Some(Callee::StdLib {
                    name: call.name.node.clone(),
                })
            } else if self.const_names.contains(&call.name.node) {
                Some(Callee::GlobalValue)
            } else {
                None
            }
        });
        match callee {
            Some(Callee::LocalValue) | Some(Callee::GlobalValue) => {
                let func = self.lower_var(&call.name, call.name.span, ctx, None)?;
                let mut args = Vec::new();
                for a in &call.args {
                    args.push(self.lower_expr(&a.value, ctx)?);
                }
                let rty = match &func.ty {
                    CirType::Fun(_, r) => (**r).clone(),
                    _ => return self.err(span, "call of a non-function value"),
                };
                Some(CirExpr::new(
                    CirExprKind::App {
                        func: Box::new(func),
                        args,
                    },
                    rty,
                    span,
                ))
            }
            Some(Callee::StdLib { name }) => {
                let ordered = self.order_stdlib_args(&name, &call.args, span)?;
                let mut args: Vec<CirExpr> = Vec::new();
                for (i, a) in ordered.iter().enumerate() {
                    let arg_expected = self.stdlib_arg_expected(&name, i, &args, a.span, ctx);
                    args.push(self.lower_expr_ex(a, ctx, arg_expected.as_ref())?);
                }
                let ty = self.stdlib_result(&name, &args, expected, span)?;
                Some(CirExpr::new(
                    CirExprKind::Call {
                        callee: CirCallee::StdLib { name },
                        args,
                    },
                    ty,
                    span,
                ))
            }
            Some(Callee::Operator {
                name,
                level,
                module_local: true,
            }) => {
                let op = self.ops.get(&name).copied()?;
                let subst: Subst = if op.type_params.is_empty() {
                    Subst::new()
                } else {
                    self.typed
                        .instantiations
                        .get(&call.name.span)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .collect()
                };
                let ordered = self.order_op_args(op, &call.args, span)?;
                let sig = self.typed.operator_sigs.get(&name).cloned();
                let mut args = Vec::new();
                for (i, a) in ordered.iter().enumerate() {
                    let arg_expected = sig
                        .as_ref()
                        .and_then(|s| s.params.get(i))
                        .and_then(|(_, t)| self.cir_ty(&subst_ty(t, &subst), &subst));
                    args.push(self.lower_expr_ex(a, ctx, arg_expected.as_ref())?);
                }
                let ty = sig
                    .and_then(|s| self.cir_ty(&subst_ty(&s.ret, &subst), &subst))
                    .or_else(|| expected.cloned())
                    .or_else(|| self.err(span, "cannot derive the result type of this call"))?;
                self.enqueue(&name, subst.clone());
                Some(CirExpr::new(
                    CirExprKind::Call {
                        callee: CirCallee::Operator {
                            name: mangle(&name, &subst),
                            level,
                        },
                        args,
                    },
                    ty,
                    span,
                ))
            }
            Some(Callee::Operator {
                name,
                level,
                module_local: false,
            }) => {
                // Cross-module call: the callee lives in a dependency's
                // generated Rust module; reference it by qualified path.
                let Some((rust_mod, isig)) = self.imported_ops.get(&name).cloned() else {
                    return self.err(
                        span,
                        format!(
                            "cross-module call to `{}`: the dependency's public interface is unavailable",
                            name
                        ),
                    );
                };
                if level != EffectLevel::Function {
                    return self.err(
                        span,
                        format!(
                            "cross-module call to `{}`: calls to imported queries/actions are not supported by the rust backend yet",
                            name
                        ),
                    );
                }
                if !isig.type_params.is_empty() {
                    return self.err(
                        span,
                        format!(
                            "cross-module call to generic function `{}` is not supported yet",
                            name
                        ),
                    );
                }
                // Nominal types of the dependency (enums, table rows) cannot
                // be named from this module's generated code.
                let sig_tys = isig
                    .sig
                    .params
                    .iter()
                    .map(|(_, t)| t)
                    .chain(std::iter::once(&isig.sig.ret));
                if let Some(bad) = sig_tys.into_iter().find_map(nominal_dep_ty) {
                    return self.err(
                        span,
                        format!(
                            "cross-module call to `{}`: its signature mentions `{bad}`, a nominal type of the imported module (not supported yet)",
                            name
                        ),
                    );
                }
                let names: Vec<String> =
                    isig.sig.params.iter().map(|(n, _)| n.clone()).collect();
                let ordered = order_by_names(&names, &call.args)
                    .or_else(|| self.err(span, format!("bad argument list for `{}`", name)))?;
                let mut args = Vec::new();
                for (i, a) in ordered.iter().enumerate() {
                    let arg_expected = self.cir_ty(&isig.sig.params[i].1, &ctx.subst);
                    args.push(self.lower_expr_ex(a, ctx, arg_expected.as_ref())?);
                }
                let ty = self
                    .cir_ty(&isig.sig.ret, &ctx.subst)
                    .or_else(|| self.err(span, "cannot derive the result type of this call"))?;
                Some(CirExpr::new(
                    CirExprKind::Call {
                        callee: CirCallee::Operator {
                            name: format!("crate::{}::{}", rust_mod, sanitize(&name)),
                            level,
                        },
                        args,
                    },
                    ty,
                    span,
                ))
            }
            Some(Callee::LookupPrim) => {
                self.err(span, "`lookup` primitive survived desugaring")
            }
            None => self.err(span, format!("unresolved call to `{}`", call.name.node)),
        }
    }

    /// Expected type for one stdlib argument, computed from the arguments
    /// already lowered (only higher-order positions need this).
    fn stdlib_arg_expected(
        &mut self,
        name: &str,
        idx: usize,
        args: &[CirExpr],
        arg_span: Span,
        ctx: &Ctx,
    ) -> Option<CirType> {
        // Return type of the argument lambda as recorded by the type
        // checker (valid even when desugaring changed the receiver type).
        let lam_ret = |s: &mut Self| match s.hint_ty(arg_span, ctx) {
            Some(CirType::Fun(_, r)) => Some(*r),
            _ => None,
        };
        match (name, idx) {
            ("fold" | "scan_left", 2) => {
                let t = elem_ty(&args.first()?.ty)?;
                let a = args.get(1)?.ty.clone();
                Some(CirType::Fun(
                    Box::new(CirType::Tuple(vec![a.clone(), t])),
                    Box::new(a),
                ))
            }
            ("filter", 1) => {
                let t = elem_ty(&args.first()?.ty)?;
                Some(CirType::Fun(Box::new(t), Box::new(CirType::Bool)))
            }
            ("map" | "and_then", 1) => {
                let t = match &args.first()?.ty {
                    CirType::Option(t) => (**t).clone(),
                    other => elem_ty(other)?,
                };
                let r = lam_ret(self)?;
                Some(CirType::Fun(Box::new(t), Box::new(r)))
            }
            ("sum_by" | "avg_by", 2) => {
                let t = elem_ty(&args.first()?.ty)?;
                Some(CirType::Fun(Box::new(t), Box::new(CirType::Float)))
            }
            _ => None,
        }
    }

    /// Derive the result type of a stdlib call from its argument types.
    fn stdlib_result(
        &mut self,
        name: &str,
        args: &[CirExpr],
        expected: Option<&CirType>,
        span: Span,
    ) -> Option<CirType> {
        let fun_ret = |i: usize| -> Option<CirType> {
            match &args.get(i)?.ty {
                CirType::Fun(_, r) => Some((**r).clone()),
                _ => None,
            }
        };
        let mut agg_row = |k: CirType, a: CirType| {
            CirType::Vector(Box::new(CirType::Record(self.recs.intern(vec![
                ("key".to_string(), k),
                ("agg".to_string(), a),
            ]))))
        };
        let ty = match name {
            "contains" | "starts_with" | "ends_with" | "is_empty" | "is_some" | "is_none" => {
                CirType::Bool
            }
            "length" | "size" | "copies_in" | "map_size" | "year" | "month" | "day"
            | "day_of_week" | "days_between" => CirType::Int,
            "floor" | "ceil" | "round" => CirType::Float,
            "abs" | "min" | "max" => CirType::Int,
            "concat" | "trim" | "substring" | "join" | "to_string_int" | "to_string_float"
            | "to_string_date" | "to_string_bool" | "to_string_decimal" => CirType::String,
            "split" => CirType::Vector(Box::new(CirType::String)),
            "decimal_from_string" => match (expected, self.hint_ty(span, &Ctx::default())) {
                (Some(t @ CirType::Option(_)), _) => t.clone(),
                (_, Some(t @ CirType::Option(_))) => t,
                _ => CirType::Option(Box::new(CirType::Decimal(None))),
            },
            "round_to" => args.first()?.ty.clone(),
            "add_days" => CirType::Option(Box::new(CirType::Date)),
            "parse_date" => CirType::Option(Box::new(CirType::Date)),
            "fold" => args.get(1)?.ty.clone(),
            "scan_left" => CirType::Vector(Box::new(args.get(1)?.ty.clone())),
            "map" => {
                let r = fun_ret(1)?;
                match &args.first()?.ty {
                    CirType::Vector(_) => CirType::Vector(Box::new(r)),
                    CirType::Option(_) => CirType::Option(Box::new(r)),
                    _ => return self.err(span, "`map` on a non-vector/non-option value"),
                }
            }
            "filter" | "append" | "sort_by" | "take" | "drop" | "concat_vector" => {
                args.first()?.ty.clone()
            }
            "to_vector" => CirType::Vector(Box::new(elem_ty(&args.first()?.ty)?)),
            "to_set" => CirType::Set(Box::new(elem_ty(&args.first()?.ty)?)),
            "the" => elem_ty(&args.first()?.ty)?,
            "only" => CirType::Option(Box::new(elem_ty(&args.first()?.ty)?)),
            "union_all" => elem_ty(&args.first()?.ty)?,
            "bag_to_set" => CirType::Set(Box::new(elem_ty(&args.first()?.ty)?)),
            "set_to_bag" => CirType::Bag(Box::new(elem_ty(&args.first()?.ty)?)),
            "bag_union" => args.first()?.ty.clone(),
            "map_get" => CirType::Option(Box::new(map_val_ty(&args.first()?.ty)?)),
            "map_insert" | "map_remove" => args.first()?.ty.clone(),
            "map_keys" => CirType::Set(Box::new(map_key_ty(&args.first()?.ty)?)),
            "map_values" => CirType::Bag(Box::new(map_val_ty(&args.first()?.ty)?)),
            "map_from_vector" => match elem_ty(&args.first()?.ty)? {
                CirType::Tuple(ts) if ts.len() == 2 => {
                    CirType::Map(Box::new(ts[0].clone()), Box::new(ts[1].clone()))
                }
                _ => return self.err(span, "`map_from_vector` of non-pair vector"),
            },
            "map_to_vector" => {
                let k = map_key_ty(&args.first()?.ty)?;
                let v = map_val_ty(&args.first()?.ty)?;
                CirType::Vector(Box::new(CirType::Tuple(vec![k, v])))
            }
            "and_then" => fun_ret(1)?,
            "unwrap_or" => match &args.first()?.ty {
                CirType::Option(t) => (**t).clone(),
                _ => return self.err(span, "`unwrap_or` on a non-option value"),
            },
            "aggregate" => agg_row(fun_ret(1)?, fun_ret(5)?),
            "count_by" => agg_row(fun_ret(1)?, CirType::Int),
            "sum_by" | "avg_by" => agg_row(fun_ret(1)?, CirType::Float),
            "min_by" | "max_by" => agg_row(fun_ret(1)?, fun_ret(2)?),
            _ => return self.err(span, format!("unknown stdlib function `{name}`")),
        };
        Some(ty)
    }

    /// Order call arguments to declaration order (named-argument sugar was
    /// validated by resolve; desugaring preserves the names).
    fn order_stdlib_args<'e>(
        &mut self,
        name: &str,
        args: &'e [Arg],
        span: Span,
    ) -> Option<Vec<&'e Expr>> {
        let names = crate::resolve::stdlib_signature(name)?;
        order_by_names(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>(), args)
            .or_else(|| self.err(span, format!("bad argument list for stdlib `{name}`")))
    }

    fn order_op_args<'e>(
        &mut self,
        op: &OperatorDecl,
        args: &'e [Arg],
        span: Span,
    ) -> Option<Vec<&'e Expr>> {
        let names: Vec<String> = op.params.iter().map(|p| p.name.node.clone()).collect();
        order_by_names(&names, args)
            .or_else(|| self.err(span, format!("bad argument list for `{}`", op.name.node)))
    }

    // -- lambda lifting ---------------------------------------------------------

    /// Lift a lambda to a top-level function and return the `MakeClosure`
    /// expression constructing its value (doc/codegen-backend.md §3).
    fn lift_lambda(
        &mut self,
        e: &Expr,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<CirExpr> {
        let ExprKind::Lambda(l) = &e.kind else { unreachable!() };
        let span = e.span;
        let id = self.lift_counter;
        self.lift_counter += 1;
        let fname = format!("__lift_{id}");

        // Function type: the call-site expected type when present — it is
        // constructed for this exact lambda (read predicates, update
        // transforms, stdlib higher-order args) and tracks desugaring;
        // otherwise the side table (desugarer-synthesized lambdas share the
        // replaced expression's span and may carry a bogus non-function
        // type, which is skipped).
        let fun_ty = match (expected, self.hint_ty(span, ctx)) {
            (Some(t @ CirType::Fun(..)), _) => t.clone(),
            (None, Some(t @ CirType::Fun(..))) => t,
            _ => {
                return self.err(
                    span,
                    "cannot determine this lambda's type (no expected function type)",
                )
            }
        };
        let (pty, rty) = match fun_ty {
            CirType::Fun(p, r) => (*p, *r),
            _ => unreachable!(),
        };

        // Captured environment: values are lowered in the *enclosing* scope.
        let mut env_tys: Vec<(String, CirType)> = Vec::new();
        let mut env_vals: Vec<(String, CirExpr)> = Vec::new();
        for cap in &l.captures {
            let cty = ctx.lookup(&cap.node).cloned()?;
            env_tys.push((sanitize(&cap.node), cty.clone()));
            let cvar = self.lower_var(cap, cap.span, ctx, None)?;
            env_vals.push((sanitize(&cap.node), cvar));
            let _ = cty;
        }
        let param_tys: Vec<CirType> = if l.params.len() == 1 {
            vec![pty.clone()]
        } else {
            match &pty {
                CirType::Tuple(ts) if ts.len() == l.params.len() => ts.clone(),
                _ => return self.err(span, "lambda parameter/type arity mismatch"),
            }
        };

        // Lower the body in a scope where captures are env fields and the
        // parameters are bound.
        let mut lctx = Ctx {
            locals: vec![HashMap::new()],
            env: Some(env_tys.clone()),
            subst: ctx.subst.clone(),
        };
        let mut pats = Vec::new();
        for (p, pty) in l.params.iter().zip(&param_tys) {
            let mut wrappers = Vec::new();
            let cp = self.lower_pat(&p.pat, pty, &mut wrappers, span)?;
            if !wrappers.is_empty() {
                return self.err(span, "unsupported pattern in lambda parameter");
            }
            self.bind_pat_names(&p.pat, pty, &mut lctx, span)?;
            pats.push(cp);
        }
        let body = self.lower_expr_ex(&l.body, &mut lctx, Some(&rty))?;

        // Wrap the body in parameter destructuring lets over `__arg`.
        let mut wrapped = body;
        for (i, cp) in pats.into_iter().enumerate().rev() {
            let value = if l.params.len() == 1 {
                CirExpr::new(CirExprKind::Var("__arg".into()), pty.clone(), span)
            } else {
                CirExpr::new(
                    CirExprKind::TupleProj {
                        base: Box::new(CirExpr::new(CirExprKind::Var("__arg".into()), pty.clone(), span)),
                        index: i as u32,
                    },
                    param_tys[i].clone(),
                    span,
                )
            };
            let wty = wrapped.ty.clone();
            let wspan = wrapped.span;
            wrapped = CirExpr::new(
                CirExprKind::Let {
                    pat: cp,
                    value: Box::new(value),
                    body: Box::new(wrapped),
                },
                wty,
                wspan,
            );
        }

        let ret = rty.clone();
        self.functions.push(CirFunDef {
            name: fname.clone(),
            env: Some(env_tys),
            params: vec![("__arg".into(), pty.clone())],
            ret: rty,
            body: wrapped,
        });
        Some(CirExpr::new(
            CirExprKind::MakeClosure {
                fun: fname,
                env: env_vals,
            },
            CirType::Fun(Box::new(pty), Box::new(ret)),
            span,
        ))
    }

    // -- match / pattern compilation -------------------------------------------

    fn lower_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        ctx: &mut Ctx,
        expected: Option<&CirType>,
    ) -> Option<CirExpr> {
        let scrut = self.lower_expr(scrutinee, ctx)?;
        let sty = scrut.ty.clone();
        // §2.2: the type checker may see the scrutinee under a different
        // (record-equivalent) view than the runtime value — e.g. `lookup`
        // yields `option<value t>` (non-key fields) while the read produces
        // full rows. Bind pattern variables under the type checker's view
        // and bridge the representations with coercion lets.
        let recorded_sty = self.hint_ty(scrutinee.span, ctx).filter(|t| *t != sty);
        let mut out = Vec::new();
        for arm in arms {
            let mut wrappers: Vec<(CirPat, CirExpr)> = Vec::new();
            // Pattern compilation matches the actual runtime value, so it
            // uses the structural type.
            let cp = self.lower_pat(&arm.pat, &sty, &mut wrappers, span)?;
            ctx.locals.push(HashMap::new());
            let bind_ty = recorded_sty.as_ref().unwrap_or(&sty);
            self.bind_pat_names(&arm.pat, bind_ty, ctx, span)?;
            let mut body = self.lower_expr_ex(&arm.body, ctx, expected)?;
            ctx.locals.pop();
            if let Some(rt) = &recorded_sty {
                let mut convs = Vec::new();
                pat_view_conversions(&arm.pat, &sty, rt, &mut convs);
                for (name, from, to) in convs.into_iter().rev() {
                    let value = self.view_coercion(&name, &from, &to, span)?;
                    let bty = body.ty.clone();
                    let bspan = body.span;
                    body = CirExpr::new(
                        CirExprKind::Let {
                            pat: CirPat::Bind(sanitize(&name)),
                            value: Box::new(value),
                            body: Box::new(body),
                        },
                        bty,
                        bspan,
                    );
                }
            }
            for (wp, wv) in wrappers.into_iter().rev() {
                let wty = body.ty.clone();
                let wspan = body.span;
                body = CirExpr::new(
                    CirExprKind::Let {
                        pat: wp,
                        value: Box::new(wv),
                        body: Box::new(body),
                    },
                    wty,
                    wspan,
                );
            }
            out.push(CirArm { pat: cp, body });
        }
        let ty = expected
            .cloned()
            .or_else(|| out.first().map(|a| a.body.ty.clone()))
            .or_else(|| self.err(span, "cannot derive the type of this match"))?;
        Some(CirExpr::new(
            CirExprKind::Match {
                scrutinee: Box::new(scrut),
                arms: out,
            },
            ty,
            span,
        ))
    }

    /// Build the expression converting a pattern-bound variable `name` from
    /// the runtime representation `from` to the type checker's view `to`
    /// (§2.2 row/value-record equivalence). Currently supports row →
    /// non-key-field record (the `lookup` boundary).
    fn view_coercion(
        &mut self,
        name: &str,
        from: &CirType,
        to: &CirType,
        span: Span,
    ) -> Option<CirExpr> {
        let CirType::Record(rec) = to else { return None };
        let fields = self.recs.fields_of(rec)?.to_vec();
        let base = CirExpr::new(CirExprKind::Var(sanitize(name)), from.clone(), span);
        let mut out = Vec::new();
        for (fname, fty) in fields {
            let access = CirExpr::new(
                CirExprKind::Field {
                    base: Box::new(base.clone()),
                    name: fname.clone(),
                },
                fty,
                span,
            );
            out.push((fname, access));
        }
        Some(CirExpr::new(
            CirExprKind::RecordLit {
                def: rec.clone(),
                fields: out,
            },
            to.clone(),
            span,
        ))
    }

    /// Compile a pattern against a value of type `ty`. Nested patterns at
    /// boxed (self-recursive enum payload) positions are rebound through
    /// deref-lets pushed to `wrappers`.
    fn lower_pat(
        &mut self,
        pat: &Pattern,
        ty: &CirType,
        wrappers: &mut Vec<(CirPat, CirExpr)>,
        span: Span,
    ) -> Option<CirPat> {
        self.lower_pat_boxed(pat, ty, false, wrappers, span)
    }

    fn lower_pat_boxed(
        &mut self,
        pat: &Pattern,
        ty: &CirType,
        boxed: bool,
        wrappers: &mut Vec<(CirPat, CirExpr)>,
        span: Span,
    ) -> Option<CirPat> {
        if boxed {
            // A pattern under a Box: bind a fresh variable and destructure
            // through a deref-let appended to the wrappers.
            match &pat.kind {
                PatternKind::Wildcard => return Some(CirPat::Wildcard),
                PatternKind::Bind(n) => {
                    wrappers.push((
                        CirPat::Bind(sanitize(&n.node)),
                        CirExpr::new(
                            CirExprKind::Deref(Box::new(CirExpr::new(
                                CirExprKind::Var(sanitize(&n.node)),
                                ty.clone(),
                                pat.span,
                            ))),
                            ty.clone(),
                            pat.span,
                        ),
                    ));
                    return Some(CirPat::Bind(sanitize(&n.node)));
                }
                _ => {
                    let fresh = format!("__b{}", self.fresh());
                    let sub = self.lower_pat_boxed(pat, ty, false, wrappers, span)?;
                    wrappers.push((
                        sub,
                        CirExpr::new(
                            CirExprKind::Deref(Box::new(CirExpr::new(
                                CirExprKind::Var(fresh.clone()),
                                ty.clone(),
                                pat.span,
                            ))),
                            ty.clone(),
                            pat.span,
                        ),
                    ));
                    return Some(CirPat::Bind(fresh));
                }
            }
        }
        let cp = match &pat.kind {
            PatternKind::Wildcard => CirPat::Wildcard,
            PatternKind::Bind(n) => CirPat::Bind(sanitize(&n.node)),
            PatternKind::Lit(l) => CirPat::Lit(l.clone()),
            PatternKind::None => CirPat::None,
            PatternKind::Some(inner) => {
                let inner_ty = match ty {
                    CirType::Option(t) => (**t).clone(),
                    _ => return self.err(pat.span, "`some` pattern on non-option value"),
                };
                CirPat::Some(Box::new(self.lower_pat_boxed(
                    inner, &inner_ty, false, wrappers, span,
                )?))
            }
            PatternKind::Variant { name, args } => {
                let info = self.variants.get(&name.node).cloned()?;
                let mut cargs = Vec::new();
                if let Some(rec) = &info.record {
                    // Record payload: the single argument is a record pattern.
                    let [arg] = args.as_slice() else {
                        return self.err(pat.span, "record-payload variant pattern arity");
                    };
                    cargs.push(self.lower_pat_boxed(
                        arg,
                        &CirType::Record(rec.clone()),
                        false,
                        wrappers,
                        span,
                    )?);
                } else {
                    let boxed: Vec<bool> = info
                        .tys
                        .iter()
                        .map(|t| mentions_enum(t, &info.enum_rust))
                        .collect();
                    for (i, arg) in args.iter().enumerate() {
                        let Some(pty) = info.tys.get(i) else {
                            return self.err(pat.span, "variant pattern arity mismatch");
                        };
                        cargs.push(self.lower_pat_boxed(
                            arg,
                            pty,
                            boxed[i],
                            wrappers,
                            span,
                        )?);
                    }
                }
                CirPat::Variant {
                    def: info.enum_rust.clone(),
                    variant: pascal(&name.node),
                    args: cargs,
                }
            }
            PatternKind::Tuple(pats) => {
                let CirType::Tuple(ts) = ty else {
                    return self.err(pat.span, "tuple pattern on non-tuple value");
                };
                let mut out = Vec::new();
                for (p, t) in pats.iter().zip(ts) {
                    out.push(self.lower_pat_boxed(p, t, false, wrappers, span)?);
                }
                CirPat::Tuple(out)
            }
            PatternKind::Record(ids) => {
                let def = match ty {
                    CirType::Record(r) => r.clone(),
                    CirType::Row(r) => r.clone(),
                    _ => return self.err(pat.span, "record pattern on non-record value"),
                };
                CirPat::Record {
                    def,
                    fields: ids.iter().map(|i| sanitize(&i.node)).collect(),
                }
            }
            PatternKind::ConsNil | PatternKind::Cons { .. } => {
                return self.err(
                    pat.span,
                    "cons patterns (`[]` / `h :: t`) are not supported by codegen MVP",
                )
            }
        };
        Some(cp)
    }

    /// Register a pattern's bindings (with their types) in the current scope.
    fn bind_pat_names(
        &mut self,
        pat: &Pattern,
        ty: &CirType,
        ctx: &mut Ctx,
        span: Span,
    ) -> Option<()> {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Lit(_) | PatternKind::None
            | PatternKind::ConsNil => Some(()),
            PatternKind::Bind(n) => {
                ctx.bind(sanitize(&n.node), ty.clone());
                Some(())
            }
            PatternKind::Some(inner) => {
                let CirType::Option(t) = ty else {
                    return self.err(span, "`some` pattern on non-option value");
                };
                self.bind_pat_names(inner, t, ctx, span)
            }
            PatternKind::Variant { name, args } => {
                let info = self.variants.get(&name.node).cloned()?;
                if let Some(rec) = &info.record {
                    if let [arg] = args.as_slice() {
                        return self.bind_pat_names(arg, &CirType::Record(rec.clone()), ctx, span);
                    }
                    return None;
                }
                for (i, arg) in args.iter().enumerate() {
                    let pty = info.tys.get(i)?;
                    self.bind_pat_names(arg, pty, ctx, span)?;
                }
                Some(())
            }
            PatternKind::Tuple(pats) => {
                let CirType::Tuple(ts) = ty else {
                    return self.err(span, "tuple pattern on non-tuple value");
                };
                for (p, t) in pats.iter().zip(ts) {
                    self.bind_pat_names(p, t, ctx, span)?;
                }
                Some(())
            }
            PatternKind::Record(ids) => {
                let fields: &[(String, CirType)] = match ty {
                    CirType::Record(r) => {
                        match self.recs.fields_of(r) {
                            Some(fs) => fs,
                            None => return self.err(span, "unknown record type in pattern"),
                        }
                    }
                    CirType::Row(_) => {
                        // Row fields come from the table declaration.
                        let fname = self
                            .tables
                            .iter()
                            .find(|(_, d)| row_struct(&d.name.node) == *match ty {
                                CirType::Row(r) => r,
                                _ => unreachable!(),
                            })
                            .map(|(n, _)| n.clone());
                        let Some(tname) = fname else {
                            return self.err(span, "unknown row type in pattern");
                        };
                        let decl = self.tables[&tname];
                        for id in ids {
                            if let Some((_, fty)) =
                                decl.fields.iter().find(|(n, _)| n.node == id.node)
                            {
                                if let Some(ct) = self.surface_ty(fty, &Subst::new()) {
                                    ctx.bind(sanitize(&id.node), ct);
                                }
                            }
                        }
                        return Some(());
                    }
                    _ => return self.err(span, "record pattern on non-record value"),
                };
                for id in ids {
                    if let Some((_, fty)) = fields.iter().find(|(n, _)| n == &id.node) {
                        ctx.bind(sanitize(&id.node), fty.clone());
                    }
                }
                Some(())
            }
            PatternKind::Cons { .. } => {
                self.err(span, "cons patterns are not supported by codegen MVP")
            }
        }
    }

    // -- reads -------------------------------------------------------------------

    /// Lower a `read` primitive: lift the predicate, then split off the
    /// usable key equalities for point-lookup / index plans (§5.5).
    fn lower_read(
        &mut self,
        table: &Ident,
        predicate: &Expr,
        span: Span,
        ctx: &mut Ctx,
    ) -> Option<CirExpr> {
        let plan = self.m.plans.get(&span).cloned().unwrap_or(ReadPlan::FullScan);
        let row = row_struct(&table.node);
        let pred_expected = CirType::Fun(
            Box::new(CirType::Row(row.clone())),
            Box::new(CirType::Bool),
        );
        let pred = self.lower_expr_ex(predicate, ctx, Some(&pred_expected))?;
        // Key equalities: columns constrained by `row.c = e` (e row-free),
        // reusing the same decomposition as the optimize pass.
        let mut eqs: HashMap<String, &Expr> = HashMap::new();
        if let ExprKind::Lambda(lam) = &predicate.kind {
            if let [param] = lam.params.as_slice() {
                if let PatternKind::Bind(row_var) = &param.pat.kind {
                    let mut body: &Expr = &lam.body;
                    while let ExprKind::Let { body: b, .. } = &body.kind {
                        body = b;
                    }
                    let mut conjuncts = Vec::new();
                    split_ands(body, &mut conjuncts);
                    for c in conjuncts {
                        if let Some((col, rhs)) = usable_eq(c, &row_var.node) {
                            eqs.insert(col.to_string(), rhs);
                        }
                    }
                }
            }
        }
        let cols: Vec<String> = match &plan {
            ReadPlan::PointLookup => self
                .tables
                .get(&table.node)
                .map(|t| t.pk.iter().map(|c| c.node.clone()).collect())
                .unwrap_or_default(),
            ReadPlan::IndexScan { index } => self
                .indexes
                .get(&table.node)
                .and_then(|ixs| ixs.iter().find(|(n, _)| n == &index.node))
                .map(|(_, cols)| cols.clone())
                .unwrap_or_default(),
            ReadPlan::FullScan => vec![],
        };
        let mut key = Vec::new();
        for col in &cols {
            if let Some(rhs) = eqs.get(col) {
                key.push((col.clone(), self.lower_expr(rhs, ctx)?));
            }
        }
        Some(CirExpr::new(
            CirExprKind::Read {
                table: table.node.clone(),
                plan,
                key,
                predicate: Box::new(pred),
            },
            CirType::Set(Box::new(CirType::Row(row))),
            span,
        ))
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Walk a pattern in parallel with the runtime (`from`) and type-checker
/// (`to`) views of the matched value, collecting the bound variables whose
/// representations differ and need a coercion let (§2.2). Only the shapes
/// the desugarer produces for option/tuple destructuring are bridged;
/// anything else keeps the runtime representation.
fn pat_view_conversions(
    pat: &Pattern,
    from: &CirType,
    to: &CirType,
    out: &mut Vec<(String, CirType, CirType)>,
) {
    match (&pat.kind, from, to) {
        (PatternKind::Bind(n), f, t) if f != t => {
            out.push((n.node.clone(), f.clone(), t.clone()));
        }
        (PatternKind::Some(inner), CirType::Option(f), CirType::Option(t)) => {
            pat_view_conversions(inner, f, t, out);
        }
        (PatternKind::Tuple(ps), CirType::Tuple(fs), CirType::Tuple(ts))
            if ps.len() == fs.len() && ps.len() == ts.len() =>
        {
            for ((p, f), t) in ps.iter().zip(fs).zip(ts) {
                pat_view_conversions(p, f, t, out);
            }
        }
        _ => {}
    }
}

/// Order call arguments to parameter declaration order (named args matched
/// by name, positional args filling the remaining slots left to right).
fn order_by_names<'e>(names: &[String], args: &'e [Arg]) -> Option<Vec<&'e Expr>> {
    let mut out: Vec<Option<&'e Expr>> = vec![None; names.len()];
    let mut pos = 0;
    for a in args {
        match &a.name {
            Some(n) => {
                let i = names.iter().position(|p| p == &n.node)?;
                out[i] = Some(&a.value);
            }
            None => {
                while pos < names.len() && out[pos].is_some() {
                    pos += 1;
                }
                if pos >= names.len() {
                    return None;
                }
                out[pos] = Some(&a.value);
                pos += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// Find a nominal type of a dependency module (enum or table row) inside an
/// imported signature type; such types cannot be named from the importing
/// module's generated code and are rejected for now.
fn nominal_dep_ty(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Enum { name, .. } => Some(format!("enum `{name}`")),
        Ty::Row(n) => Some(format!("row type of table `{n}`")),
        Ty::Option(t) | Ty::Vector(t) | Ty::Set(t) | Ty::Bag(t) => nominal_dep_ty(t),
        Ty::Map(k, v) | Ty::Fun(k, v) => nominal_dep_ty(k).or_else(|| nominal_dep_ty(v)),
        Ty::Tuple(ts) => ts.iter().find_map(nominal_dep_ty),
        Ty::Record(fs) => fs.iter().find_map(|(_, t)| nominal_dep_ty(t)),
        _ => None,
    }
}

/// Substitute type parameters in an elaborated type.
fn subst_ty(ty: &Ty, subst: &Subst) -> Ty {
    if subst.is_empty() {
        return ty.clone();
    }
    match ty {
        Ty::Var(n) => subst.get(n).cloned().unwrap_or(ty.clone()),
        Ty::Option(t) => Ty::Option(Box::new(subst_ty(t, subst))),
        Ty::Vector(t) => Ty::Vector(Box::new(subst_ty(t, subst))),
        Ty::Set(t) => Ty::Set(Box::new(subst_ty(t, subst))),
        Ty::Bag(t) => Ty::Bag(Box::new(subst_ty(t, subst))),
        Ty::Map(k, v) => Ty::Map(Box::new(subst_ty(k, subst)), Box::new(subst_ty(v, subst))),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst_ty(t, subst)).collect()),
        Ty::Record(fs) => Ty::Record(
            fs.iter()
                .map(|(n, t)| (n.clone(), subst_ty(t, subst)))
                .collect(),
        ),
        Ty::Enum { name, args } => Ty::Enum {
            name: name.clone(),
            args: args.iter().map(|t| subst_ty(t, subst)).collect(),
        },
        Ty::Fun(a, b) => Ty::Fun(Box::new(subst_ty(a, subst)), Box::new(subst_ty(b, subst))),
        other => other.clone(),
    }
}

/// Does this CIR type mention the given enum (recursion ⇒ boxing)?
fn mentions_enum(ty: &CirType, name: &str) -> bool {
    match ty {
        CirType::Enum(n) => n == name,
        CirType::Option(t) | CirType::Vector(t) | CirType::Set(t) | CirType::Bag(t) => {
            mentions_enum(t, name)
        }
        CirType::Map(k, v) => mentions_enum(k, name) || mentions_enum(v, name),
        CirType::Tuple(ts) => ts.iter().any(|t| mentions_enum(t, name)),
        CirType::Fun(a, b) => mentions_enum(a, name) || mentions_enum(b, name),
        _ => false,
    }
}

/// Mangle an operator name with its generic instantiation (identity for
/// non-generic operators).
fn mangle(name: &str, subst: &Subst) -> String {
    if subst.is_empty() {
        return sanitize(name);
    }
    let mut keys: Vec<&String> = subst.keys().collect();
    keys.sort();
    let mut out = sanitize(name);
    for k in keys {
        out.push_str("__");
        out.push_str(&ty_mangle(&subst[k]));
    }
    out
}

fn ty_mangle(ty: &Ty) -> String {
    match ty {
        Ty::Bool => "bool".into(),
        Ty::Int => "int".into(),
        Ty::Float => "float".into(),
        Ty::Decimal(Some((m, n))) => format!("dec{m}_{n}"),
        Ty::Decimal(None) => "dec".into(),
        Ty::String => "str".into(),
        Ty::Date => "date".into(),
        Ty::Option(t) => format!("opt_{}", ty_mangle(t)),
        Ty::Vector(t) => format!("vec_{}", ty_mangle(t)),
        Ty::Set(t) => format!("set_{}", ty_mangle(t)),
        Ty::Bag(t) => format!("bag_{}", ty_mangle(t)),
        Ty::Map(k, v) => format!("map_{}_{}", ty_mangle(k), ty_mangle(v)),
        Ty::Tuple(ts) => format!("tup{}", ts.iter().map(ty_mangle).collect::<Vec<_>>().join("_")),
        Ty::Record(fs) => format!("rec_{:x}", {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in format!("{:?}", fs).as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }),
        Ty::Row(t) => format!("row_{t}"),
        Ty::Enum { name, .. } => format!("enum_{name}"),
        Ty::Fun(a, b) => format!("fun_{}_{}", ty_mangle(a), ty_mangle(b)),
        Ty::WriteOp => "writeop".into(),
        Ty::Var(n) => format!("var_{n}"),
        Ty::Error => "err".into(),
    }
}

/// PascalCase for type/variant names.
pub(crate) fn pascal(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

pub(crate) fn row_struct(table: &str) -> String {
    format!("{}Row", pascal(table))
}

pub(crate) fn key_struct(table: &str) -> String {
    format!("{}Key", pascal(table))
}

/// Sanitize a CQL identifier into a Rust identifier (keywords get a prefix).
pub(crate) fn sanitize(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
        "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
        "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "union", "box", "try", "gen",
    ];
    if KEYWORDS.contains(&name) {
        format!("cql_{name}")
    } else {
        name.to_string()
    }
}

/// Split a (possibly tuple) parameter type into per-argument expected types.
fn split_tuple(ty: &CirType, n: usize) -> Vec<Option<CirType>> {
    if n == 1 {
        return vec![Some(ty.clone())];
    }
    match ty {
        CirType::Tuple(ts) if ts.len() == n => ts.iter().map(|t| Some(t.clone())).collect(),
        _ => vec![None; n],
    }
}

/// Element type of a collection type.
fn elem_ty(ty: &CirType) -> Option<CirType> {
    match ty {
        CirType::Vector(t) | CirType::Set(t) | CirType::Bag(t) => Some((**t).clone()),
        _ => None,
    }
}

fn map_key_ty(ty: &CirType) -> Option<CirType> {
    match ty {
        CirType::Map(k, _) => Some((**k).clone()),
        _ => None,
    }
}

fn map_val_ty(ty: &CirType) -> Option<CirType> {
    match ty {
        CirType::Map(_, v) => Some((**v).clone()),
        _ => None,
    }
}

/// Does this elaborated type contain error placeholders (or unsubstituted
/// type parameters)? Such side-table entries are ignored by the lowerer.
fn contains_error(ty: &Ty) -> bool {
    match ty {
        Ty::Error | Ty::Var(_) => true,
        Ty::Option(t) | Ty::Vector(t) | Ty::Set(t) | Ty::Bag(t) => contains_error(t),
        Ty::Map(k, v) => contains_error(k) || contains_error(v),
        Ty::Tuple(ts) => ts.iter().any(contains_error),
        Ty::Record(fs) => fs.iter().any(|(_, t)| contains_error(t)),
        Ty::Enum { args, .. } => args.iter().any(contains_error),
        Ty::Fun(a, b) => contains_error(a) || contains_error(b),
        _ => false,
    }
}

/// Top-level `/\` decomposition (mirrors optimize.rs).
fn split_ands<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::BinOp {
            op: BinOpKind::And,
            lhs,
            rhs,
        } => {
            split_ands(lhs, out);
            split_ands(rhs, out);
        }
        _ => out.push(e),
    }
}

/// A conjunct `row.c = e` / `e = row.c` with `e` row-free gives the key
/// expression for column `c` (mirrors optimize.rs).
fn usable_eq<'e>(e: &'e Expr, row: &str) -> Option<(&'e str, &'e Expr)> {
    let ExprKind::BinOp {
        op: BinOpKind::Eq,
        lhs,
        rhs,
    } = &e.kind
    else {
        return None;
    };
    for (a, b) in [(lhs, rhs), (rhs, lhs)] {
        if let ExprKind::Field { base, name } = &a.kind {
            if matches!(&base.kind, ExprKind::Var(u) if u.node == row) && !refs_row(b, row) {
                return Some((name.node.as_str(), b));
            }
        }
    }
    None
}

fn refs_row(e: &Expr, row: &str) -> bool {
    match &e.kind {
        ExprKind::Var(u) => u.node == row,
        _ => {
            let mut found = false;
            crate::terminate::walk_children(e, &mut |child| {
                if !found && refs_row(child, row) {
                    found = true;
                }
            });
            found
        }
    }
}

/// Is this pattern irrefutable (always matches)?
pub(crate) fn is_irrefutable(p: &CirPat) -> bool {
    match p {
        CirPat::Wildcard | CirPat::Bind(_) => true,
        CirPat::Tuple(ps) => ps.iter().all(is_irrefutable),
        CirPat::Record { .. } => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lower_ok(src: &str) -> CirModule {
        let (opt, bag) = crate::pipeline::compile_module(src);
        assert!(!bag.has_errors(), "{}", bag.render());
        let opt = opt.expect("optimized module");
        match lower_to_cir(&opt) {
            Ok(m) => m,
            Err(bag) => panic!("CIR lowering failed:\n{}", bag.render()),
        }
    }

    fn find_read(e: &CirExpr) -> Option<ReadPlan> {
        let mut found = None;
        let mut check = |x: &CirExpr| {
            if found.is_none() {
                if let CirExprKind::Read { plan, .. } = &x.kind {
                    found = Some(plan.clone());
                }
            }
        };
        check(e);
        walk_cir(e, &mut check);
        found
    }

    /// Shallow child walk for test assertions.
    fn walk_cir(e: &CirExpr, f: &mut impl FnMut(&CirExpr)) {
        let kids: Vec<&CirExpr> = match &e.kind {
            CirExprKind::App { func, args } => {
                let mut v = vec![func.as_ref()];
                v.extend(args.iter());
                v
            }
            CirExprKind::Call { args, .. } => args.iter().collect(),
            CirExprKind::MakeClosure { env, .. } => env.iter().map(|(_, x)| x).collect(),
            CirExprKind::Let { value, body, .. } => vec![value, body],
            CirExprKind::If {
                cond,
                then_br,
                else_br,
            } => vec![cond, then_br, else_br],
            CirExprKind::Match { scrutinee, arms } => {
                let mut v = vec![scrutinee.as_ref()];
                v.extend(arms.iter().map(|a| &a.body));
                v
            }
            CirExprKind::RecordLit { fields, .. } => fields.iter().map(|(_, x)| x).collect(),
            CirExprKind::RecordUpd { base, fields, .. } => {
                let mut v = vec![base.as_ref()];
                v.extend(fields.iter().map(|(_, x)| x));
                v
            }
            CirExprKind::Tuple(xs)
            | CirExprKind::Vector(xs)
            | CirExprKind::Set(xs)
            | CirExprKind::Bag(xs) => xs.iter().collect(),
            CirExprKind::MapLit(kvs) => kvs.iter().flat_map(|(k, v)| [k, v]).collect(),
            CirExprKind::OptionSome(x) | CirExprKind::Deref(x) => vec![x.as_ref()],
            CirExprKind::BinOp { lhs, rhs, .. } => vec![lhs.as_ref(), rhs.as_ref()],
            CirExprKind::UnOp { operand, .. } => vec![operand.as_ref()],
            CirExprKind::Field { base, .. } | CirExprKind::TupleProj { base, .. } => {
                vec![base.as_ref()]
            }
            CirExprKind::EnumConstruct { args, .. } => args.iter().collect(),
            CirExprKind::Cast { expr, .. } => vec![expr.as_ref()],
            CirExprKind::Read { key, predicate, .. } => {
                let mut v: Vec<&CirExpr> = key.iter().map(|(_, x)| x).collect();
                v.push(predicate);
                v
            }
            CirExprKind::WriteOp(w) => match w {
                CirWriteOp::Insert { row, .. } => vec![row.as_ref()],
                CirWriteOp::Update {
                    key, transform, ..
                } => vec![key.as_ref(), transform.as_ref()],
                CirWriteOp::Delete { key, .. } => vec![key.as_ref()],
            },
            _ => vec![],
        };
        for k in kids {
            f(k);
            walk_cir(k, f);
        }
    }

    #[test]
    fn lambda_lifting_produces_top_level_funs() {
        let m = lower_ok(
            "module t;
             function apply_twice(f: int -> int, x: int) -> int == { f(f(x)) }
             query use_it() -> vector<int> == {
                 [1, 2].map(lambda(x) { x + 1 })
             }",
        );
        // One L0 function + one lifted lambda.
        let lifted: Vec<_> = m
            .functions
            .iter()
            .filter(|f| f.name.starts_with("__lift_"))
            .collect();
        assert_eq!(lifted.len(), 1, "{:?}", m.functions);
        assert_eq!(lifted[0].env, Some(vec![]));
        // The call site constructs a closure of the lifted function.
        let op = &m.operators[0];
        let mut has_closure = false;
        walk_cir(&op.body, &mut |e| {
            if matches!(&e.kind, CirExprKind::MakeClosure { fun, .. } if fun == &lifted[0].name) {
                has_closure = true;
            }
        });
        assert!(has_closure);
    }

    #[test]
    fn lambda_captures_become_env_fields() {
        let m = lower_ok(
            "module t;
             query add_n(n: int) -> vector<int> == {
                 [1, 2].map(lambda [n](x) { x + n })
             }",
        );
        let lifted = m
            .functions
            .iter()
            .find(|f| f.name.starts_with("__lift_"))
            .expect("lifted lambda");
        assert_eq!(lifted.env.as_ref().unwrap().len(), 1);
        assert_eq!(lifted.env.as_ref().unwrap()[0].0, "n");
        // The body references the capture via EnvGet.
        let mut has_env_get = false;
        walk_cir(&lifted.body, &mut |e| {
            if matches!(&e.kind, CirExprKind::EnvGet(n) if n == "n") {
                has_env_get = true;
            }
        });
        assert!(has_env_get);
    }

    #[test]
    fn read_plan_survives_into_cir() {
        let m = lower_ok(
            "module t;
             table users { id: int, name: string } primary key {id}
             query f(user_id: int) -> set<users> == {
                 read(users, lambda [user_id](u) { u.id = user_id })
             }",
        );
        let op = &m.operators[0];
        match find_read(&op.body) {
            Some(ReadPlan::PointLookup) => {}
            other => panic!("expected PointLookup, got {:?}", other),
        }
        // The key equality was extracted.
        if let CirExprKind::Read { key, .. } = &op.body.kind {
            assert_eq!(key.len(), 1);
            assert_eq!(key[0].0, "id");
        } else {
            panic!("operator body is not a Read: {:?}", op.body.kind);
        }
    }

    #[test]
    fn match_compiles_to_cir_arms() {
        let m = lower_ok(
            "module t;
             enum tree { leaf(int), node(tree, int, tree) }
             function recursive inorder(t: tree) -> vector<int> == {
                 match t {
                     leaf(v)       => [v],
                     node(l, x, r) => concat_vector(concat_vector(inorder(l), [x]), inorder(r))
                 }
             }",
        );
        let f = m.functions.iter().find(|f| f.name == "inorder").unwrap();
        let CirExprKind::Match { arms, .. } = &f.body.kind else {
            panic!("expected match, got {:?}", f.body.kind);
        };
        assert_eq!(arms.len(), 2);
        // The recursive enum marks self-references for boxing.
        let tree = m.enums.iter().find(|e| e.name == "Tree").unwrap();
        match &tree.variants[1].payload {
            CirVariantPayload::Tuple(_, boxed) => {
                assert_eq!(boxed, &[true, false, true])
            }
            p => panic!("expected tuple payload, got {:?}", p),
        }
    }

    #[test]
    fn generic_operator_is_monomorphized() {
        let m = lower_ok(
            "module t;
             function id<T>(x: T) -> T == { x }
             query use() -> int == { id::<int>(41) + id::<int>(1) }",
        );
        let names: Vec<&str> = m.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"id__int"), "{:?}", names);
        assert!(!names.contains(&"id"), "{:?}", names);
    }

    #[test]
    fn analytics_lowers_cleanly() {
        let src = include_str!("../../../examples/analytics.cql");
        let m = lower_ok(src);
        assert_eq!(m.tables.len(), 4);
        assert!(!m.operators.is_empty());
        assert_eq!(m.tests.len(), 1);
    }

    #[test]
    fn bank_lowers_cleanly() {
        let src = include_str!("../../../examples/bank_project/src/bank.cql");
        let m = lower_ok(src);
        assert_eq!(m.tables.len(), 1);
        assert_eq!(m.operators.len(), 2); // transfer + total_balance
        assert_eq!(m.tests.len(), 1);
    }
}
