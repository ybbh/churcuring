//! Type checking (doc/cql.md §2.1–2.5, §3.2–3.3, §3.6, §4.1–4.10, appendix B;
//! pipeline §D.3).
//!
//! Bidirectional local type inference over a [`ResolvedModule`]: [`Checker::check`]
//! propagates expected types into literals, `none`, empty collections, record
//! literals and lambdas; [`Checker::infer`] synthesizes. Generic functions
//! (stdlib appendix B and user `function`s) are instantiated at each call site
//! by left-to-right matching of argument types against parameter schemes —
//! local matching, not global Hindley-Milner (§2.5).
//!
//! The pass never aborts: errors accumulate in a [`DiagBag`] and the
//! [`Ty::Error`] placeholder suppresses cascades, so each operator body is
//! checked independently of failures in the others.
//!
//! Output: a [`TypedModule`] — the resolved module plus side tables:
//! expression types keyed by span (carrying decimal precisions and enum
//! instantiations), call-site generic instantiations keyed by callee span,
//! and per-operator signatures / local-binding summaries for desugaring and
//! codegen.

use std::collections::{HashMap, HashSet};
use std::fmt;

use miette::NamedSource;

use crate::ast::*;
use crate::diag::{CqlError, DiagBag};
use crate::resolve::{stdlib_signature, Callee, ResolvedModule, VarRes};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A type during type checking (distinct from the surface [`Type`] syntax).
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Bool,
    Int,
    Float,
    /// `decimal(m, n)`; `None` = unbounded decimal (value carries its scale).
    Decimal(Option<(u32, u32)>),
    String,
    Date,
    Option(Box<Ty>),
    Vector(Box<Ty>),
    Set(Box<Ty>),
    Bag(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Tuple(Vec<Ty>),
    Record(Vec<(String, Ty)>),
    /// Pure function type; multi-argument functions take a [`Ty::Tuple`].
    Fun(Box<Ty>, Box<Ty>),
    /// A declared enum with its generic arguments (empty if not generic).
    Enum { name: String, args: Vec<Ty> },
    /// A table's row type; record-equivalent via the table's field list (§2.2).
    Row(String),
    /// The type-erased runtime write descriptor (§3.6).
    WriteOp,
    /// A rigid type parameter of the enclosing generic `function`.
    Var(String),
    /// Placeholder produced after a reported error; unifies with anything.
    Error,
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Bool => write!(f, "bool"),
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Decimal(None) => write!(f, "decimal"),
            Ty::Decimal(Some((m, n))) => write!(f, "decimal({}, {})", m, n),
            Ty::String => write!(f, "string"),
            Ty::Date => write!(f, "date"),
            Ty::Option(t) => write!(f, "option<{}>", t),
            Ty::Vector(t) => write!(f, "vector<{}>", t),
            Ty::Set(t) => write!(f, "set<{}>", t),
            Ty::Bag(t) => write!(f, "bag<{}>", t),
            Ty::Map(k, v) => write!(f, "map<{}, {}>", k, v),
            Ty::Tuple(ts) => {
                let inner: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
                write!(f, "({})", inner.join(", "))
            }
            Ty::Record(fs) => {
                let inner: Vec<String> = fs.iter().map(|(n, t)| format!("{}: {}", n, t)).collect();
                write!(f, "{{ {} }}", inner.join(", "))
            }
            Ty::Fun(a, b) => {
                if matches!(**a, Ty::Fun(..)) {
                    write!(f, "({}) -> {}", a, b)
                } else {
                    write!(f, "{} -> {}", a, b)
                }
            }
            Ty::Enum { name, args } => {
                if args.is_empty() {
                    write!(f, "{}", name)
                } else {
                    let inner: Vec<String> = args.iter().map(|t| t.to_string()).collect();
                    write!(f, "{}<{}>", name, inner.join(", "))
                }
            }
            Ty::Row(n) => write!(f, "{}", n),
            Ty::WriteOp => write!(f, "write_op"),
            Ty::Var(n) => write!(f, "{}", n),
            Ty::Error => write!(f, "<error>"),
        }
    }
}

// ---------------------------------------------------------------------------
// Scheme types (generic signatures)
// ---------------------------------------------------------------------------

/// A scheme type: a [`Ty`] template with unification variables, used for the
/// generic signatures of stdlib functions and user `function`s.
#[derive(Debug, Clone)]
enum STy {
    Bool,
    Int,
    Float,
    String,
    Date,
    Decimal(Option<(u32, u32)>),
    /// A decimal precision meta-variable (appendix B's `decimal(m, n)`),
    /// instantiated per call site from the actual argument's precision.
    DecMeta(&'static str),
    Option(Box<STy>),
    Vector(Box<STy>),
    Set(Box<STy>),
    Bag(Box<STy>),
    /// `set<T>` or `bag<T>` (aggregate sources, §4.8.3).
    SetOrBag(Box<STy>),
    Map(Box<STy>, Box<STy>),
    Tuple(Vec<STy>),
    Record(Vec<(String, STy)>),
    Fun(Box<STy>, Box<STy>),
    Enum { name: String, args: Vec<STy> },
    Row(String),
    WriteOp,
    /// A generic type parameter.
    Var(String),
}

type Subst = HashMap<String, Ty>;

fn dec_meta_key(name: &str) -> String {
    format!("@dec:{}", name)
}

/// A generic signature: type parameters, parameter schemes (with names, for
/// named-argument ordering), result scheme, and constraints on type
/// parameters beyond those derivable from `set`/`map` key positions.
#[derive(Debug, Clone)]
struct Scheme {
    tparams: Vec<String>,
    param_names: Vec<String>,
    params: Vec<STy>,
    ret: STy,
    /// Type params that must be `ord` (e.g. `sort_by`'s `K`).
    ord: Vec<&'static str>,
    /// Type params that must be `hashable` beyond set/map-key positions
    /// (e.g. `aggregate`'s group key `K`).
    hash: Vec<&'static str>,
}

// ---- scheme constructors ---------------------------------------------------

/// Convert an elaborated [`Ty`] into a scheme template [`STy`] (used to turn
/// a dependency's public operator signature into a callable scheme). Returns
/// `None` when the type contains error placeholders.
fn sty_of_ty(ty: &Ty) -> Option<STy> {
    Some(match ty {
        Ty::Bool => STy::Bool,
        Ty::Int => STy::Int,
        Ty::Float => STy::Float,
        Ty::Decimal(p) => STy::Decimal(*p),
        Ty::String => STy::String,
        Ty::Date => STy::Date,
        Ty::Option(t) => STy::Option(Box::new(sty_of_ty(t)?)),
        Ty::Vector(t) => STy::Vector(Box::new(sty_of_ty(t)?)),
        Ty::Set(t) => STy::Set(Box::new(sty_of_ty(t)?)),
        Ty::Bag(t) => STy::Bag(Box::new(sty_of_ty(t)?)),
        Ty::Map(k, v) => STy::Map(Box::new(sty_of_ty(k)?), Box::new(sty_of_ty(v)?)),
        Ty::Tuple(ts) => STy::Tuple(ts.iter().map(sty_of_ty).collect::<Option<Vec<_>>>()?),
        Ty::Record(fs) => STy::Record(
            fs.iter()
                .map(|(n, t)| Some((n.clone(), sty_of_ty(t)?)))
                .collect::<Option<Vec<_>>>()?,
        ),
        Ty::Fun(a, b) => STy::Fun(Box::new(sty_of_ty(a)?), Box::new(sty_of_ty(b)?)),
        Ty::Enum { name, args } => STy::Enum {
            name: name.clone(),
            args: args.iter().map(sty_of_ty).collect::<Option<Vec<_>>>()?,
        },
        Ty::Row(n) => STy::Row(n.clone()),
        Ty::WriteOp => STy::WriteOp,
        Ty::Var(n) => STy::Var(n.clone()),
        Ty::Error => return None,
    })
}

fn st_var(n: &str) -> STy {
    STy::Var(n.to_string())
}

fn st_option(t: STy) -> STy {
    STy::Option(Box::new(t))
}

fn st_vector(t: STy) -> STy {
    STy::Vector(Box::new(t))
}

fn st_set(t: STy) -> STy {
    STy::Set(Box::new(t))
}

fn st_bag(t: STy) -> STy {
    STy::Bag(Box::new(t))
}

fn st_map(k: STy, v: STy) -> STy {
    STy::Map(Box::new(k), Box::new(v))
}

fn st_tuple(ts: Vec<STy>) -> STy {
    STy::Tuple(ts)
}

fn st_fun(a: STy, b: STy) -> STy {
    STy::Fun(Box::new(a), Box::new(b))
}

fn st_agg_row(k: STy, agg: STy) -> STy {
    STy::Record(vec![("key".to_string(), k), ("agg".to_string(), agg)])
}

fn scheme(tparams: &[&str], params: Vec<STy>, ret: STy) -> Scheme {
    Scheme {
        tparams: tparams.iter().map(|s| s.to_string()).collect(),
        param_names: vec![],
        params,
        ret,
        ord: vec![],
        hash: vec![],
    }
}

/// The two `length` overloads and the two `map` overloads (appendix B:
/// "same-name dispatch exists only for `length` and `map`"), selected by the
/// receiver/first-arg type.
fn length_string_scheme() -> Scheme {
    scheme(&[], vec![STy::String], STy::Int)
}

fn length_vector_scheme() -> Scheme {
    scheme(&["T"], vec![st_vector(st_var("T"))], STy::Int)
}

fn map_vector_scheme() -> Scheme {
    scheme(
        &["A", "B"],
        vec![st_vector(st_var("A")), st_fun(st_var("A"), st_var("B"))],
        st_vector(st_var("B")),
    )
}

fn map_option_scheme() -> Scheme {
    scheme(
        &["T", "U"],
        vec![st_option(st_var("T")), st_fun(st_var("T"), st_var("U"))],
        st_option(st_var("U")),
    )
}

/// The stdlib generic signatures (appendix B). `length` and `map` are handled
/// by [`Checker::stdlib_dispatch`] instead. `aggregate` (§4.8.3) is a builtin
/// combinator but follows the same call protocol.
fn stdlib_scheme(name: &str) -> Option<Scheme> {
    use STy::*;
    let sc = match name {
        // string
        "contains" | "starts_with" | "ends_with" => {
            scheme(&[], vec![String, String], Bool)
        }
        "concat" => scheme(&[], vec![String, String], String),
        "to_string_int" => scheme(&[], vec![Int], String),
        "to_string_float" => scheme(&[], vec![Float], String),
        "to_string_date" => scheme(&[], vec![Date], String),
        "to_string_bool" => scheme(&[], vec![Bool], String),
        "to_string_decimal" => scheme(&[], vec![DecMeta("d")], String),
        "substring" => scheme(&[], vec![String, Int, Int], String),
        "trim" => scheme(&[], vec![String], String),
        "split" => scheme(&[], vec![String, String], st_vector(String)),
        "join" => scheme(&[], vec![st_vector(String), String], String),
        // math
        "abs" => scheme(&[], vec![Int], Int),
        "min" | "max" => scheme(&[], vec![Int, Int], Int),
        "floor" | "ceil" | "round" => scheme(&[], vec![Float], Float),
        // decimal
        "decimal_from_string" => scheme(&[], vec![String], st_option(DecMeta("d"))),
        "round_to" => scheme(&[], vec![DecMeta("d"), Int], DecMeta("d")),
        // date
        "year" | "month" | "day" | "day_of_week" => scheme(&[], vec![Date], Int),
        "add_days" => scheme(&[], vec![Date, Int], Date),
        "days_between" => scheme(&[], vec![Date, Date], Int),
        "parse_date" => scheme(&[], vec![String], st_option(Date)),
        // vector / iteration
        "fold" => scheme(
            &["A", "T"],
            vec![
                st_vector(st_var("T")),
                st_var("A"),
                st_fun(st_tuple(vec![st_var("A"), st_var("T")]), st_var("A")),
            ],
            st_var("A"),
        ),
        "filter" => scheme(
            &["T"],
            vec![st_vector(st_var("T")), st_fun(st_var("T"), Bool)],
            st_vector(st_var("T")),
        ),
        "append" => scheme(
            &["T"],
            vec![st_vector(st_var("T")), st_var("T")],
            st_vector(st_var("T")),
        ),
        "to_vector" => scheme(&["T"], vec![st_set(st_var("T"))], st_vector(st_var("T"))),
        "sort_by" => {
            let mut s = scheme(
                &["T", "K"],
                vec![st_vector(st_var("T")), st_fun(st_var("T"), st_var("K"))],
                st_vector(st_var("T")),
            );
            s.ord.push("K");
            s
        }
        "take" | "drop" => scheme(
            &["T"],
            vec![st_vector(st_var("T")), Int],
            st_vector(st_var("T")),
        ),
        "to_set" => scheme(&["T"], vec![st_vector(st_var("T"))], st_set(st_var("T"))),
        "is_empty" => scheme(&["T"], vec![st_vector(st_var("T"))], Bool),
        "concat_vector" => scheme(
            &["T"],
            vec![st_vector(st_var("T")), st_vector(st_var("T"))],
            st_vector(st_var("T")),
        ),
        "scan_left" => scheme(
            &["A", "T"],
            vec![
                st_vector(st_var("T")),
                st_var("A"),
                st_fun(st_tuple(vec![st_var("A"), st_var("T")]), st_var("A")),
            ],
            st_vector(st_var("A")),
        ),
        // set / bag
        "size" => scheme(&["T"], vec![st_set(st_var("T"))], Int),
        "the" => scheme(&["T"], vec![st_set(st_var("T"))], st_var("T")),
        "only" => scheme(&["T"], vec![st_set(st_var("T"))], st_option(st_var("T"))),
        "union_all" => scheme(&["T"], vec![st_set(st_set(st_var("T")))], st_set(st_var("T"))),
        "bag_to_set" => scheme(&["T"], vec![st_bag(st_var("T"))], st_set(st_var("T"))),
        "set_to_bag" => scheme(&["T"], vec![st_set(st_var("T"))], st_bag(st_var("T"))),
        "copies_in" => scheme(&["T"], vec![st_var("T"), st_bag(st_var("T"))], Int),
        "bag_union" => scheme(&["T"], vec![st_bag(st_var("T")), st_bag(st_var("T"))], st_bag(st_var("T"))),
        // map
        "map_get" => scheme(
            &["K", "V"],
            vec![st_map(st_var("K"), st_var("V")), st_var("K")],
            st_option(st_var("V")),
        ),
        "map_insert" => scheme(
            &["K", "V"],
            vec![st_map(st_var("K"), st_var("V")), st_var("K"), st_var("V")],
            st_map(st_var("K"), st_var("V")),
        ),
        "map_remove" => scheme(
            &["K", "V"],
            vec![st_map(st_var("K"), st_var("V")), st_var("K")],
            st_map(st_var("K"), st_var("V")),
        ),
        "map_keys" => scheme(&["K", "V"], vec![st_map(st_var("K"), st_var("V"))], st_set(st_var("K"))),
        "map_values" => scheme(&["K", "V"], vec![st_map(st_var("K"), st_var("V"))], st_bag(st_var("V"))),
        "map_size" => scheme(&["K", "V"], vec![st_map(st_var("K"), st_var("V"))], Int),
        "map_from_vector" => scheme(
            &["K", "V"],
            vec![st_vector(st_tuple(vec![st_var("K"), st_var("V")]))],
            st_map(st_var("K"), st_var("V")),
        ),
        "map_to_vector" => scheme(
            &["K", "V"],
            vec![st_map(st_var("K"), st_var("V"))],
            st_vector(st_tuple(vec![st_var("K"), st_var("V")])),
        ),
        // option
        "and_then" => scheme(
            &["T", "U"],
            vec![st_option(st_var("T")), st_fun(st_var("T"), st_option(st_var("U")))],
            st_option(st_var("U")),
        ),
        "unwrap_or" => scheme(&["T"], vec![st_option(st_var("T")), st_var("T")], st_var("T")),
        "is_some" | "is_none" => scheme(&["T"], vec![st_option(st_var("T"))], Bool),
        // aggregate combinator + sugars (§4.8.3)
        "aggregate" => {
            let mut s = scheme(
                &["T", "K", "V", "R"],
                vec![
                    STy::SetOrBag(Box::new(st_var("T"))),
                    st_fun(st_var("T"), st_var("K")),
                    st_fun(st_var("T"), st_var("V")),
                    st_fun(st_tuple(vec![st_var("V"), st_var("V")]), st_var("V")),
                    st_var("V"),
                    st_fun(st_var("V"), st_var("R")),
                ],
                st_vector(st_agg_row(st_var("K"), st_var("R"))),
            );
            s.hash.push("K");
            s
        }
        "count_by" => {
            let mut s = scheme(
                &["T", "K"],
                vec![STy::SetOrBag(Box::new(st_var("T"))), st_fun(st_var("T"), st_var("K"))],
                st_vector(st_agg_row(st_var("K"), Int)),
            );
            s.hash.push("K");
            s
        }
        "sum_by" | "avg_by" => {
            let mut s = scheme(
                &["T", "K"],
                vec![
                    STy::SetOrBag(Box::new(st_var("T"))),
                    st_fun(st_var("T"), st_var("K")),
                    st_fun(st_var("T"), Float),
                ],
                st_vector(st_agg_row(st_var("K"), Float)),
            );
            s.hash.push("K");
            s
        }
        "min_by" | "max_by" => {
            let mut s = scheme(
                &["T", "K", "V"],
                vec![
                    STy::SetOrBag(Box::new(st_var("T"))),
                    st_fun(st_var("T"), st_var("K")),
                    st_fun(st_var("T"), st_var("V")),
                ],
                st_vector(st_agg_row(st_var("K"), st_var("V"))),
            );
            s.hash.push("K");
            s
        }
        _ => return None,
    };
    let mut sc = sc;
    if let Some(names) = stdlib_signature(name) {
        sc.param_names = names.iter().map(|s| s.to_string()).collect();
    }
    Some(sc)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// The resolved signature of an operator, with surface types elaborated.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorSig {
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
}

/// The output of type checking: the resolved module plus type side tables.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedModule {
    pub resolved: ResolvedModule,
    /// Type of every checked expression, keyed by expression span. Carries
    /// the information later passes need: decimal precisions, enum
    /// instantiations, collection element types.
    pub expr_tys: HashMap<Span, Ty>,
    /// Generic instantiations per call site, keyed by the callee identifier
    /// span: the type arguments each type parameter was instantiated to.
    pub instantiations: HashMap<Span, Vec<(String, Ty)>>,
    /// Elaborated signature of every operator in the module.
    pub operator_sigs: HashMap<String, OperatorSig>,
    /// Summary of each operator's local bindings (parameters, let bindings,
    /// lambda parameters, pattern and generator bindings), in introduction
    /// order, keyed by operator name.
    pub operator_locals: HashMap<String, Vec<(String, Ty)>>,
}

/// Signature of a public operator of an already-compiled dependency, used
/// to type-check cross-module calls (doc/cql.md §3.1, multi-module projects).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSig {
    pub level: EffectLevel,
    pub type_params: Vec<String>,
    pub sig: OperatorSig,
}

/// Type information of the modules imported by a module under checking,
/// keyed by item name (imports bring items into scope unqualified).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportedTypes {
    pub ops: HashMap<String, ImportSig>,
    pub consts: HashMap<String, Ty>,
}

/// Type-check a resolved module with a placeholder source for diagnostics.
pub fn check_module(resolved: &ResolvedModule) -> (TypedModule, DiagBag) {
    let src = NamedSource::new(format!("{}.cql", resolved.module.name.node), String::new());
    check_module_with_src(resolved, src)
}

/// Type-check a resolved module, attaching `src` to diagnostics.
pub fn check_module_with_src(resolved: &ResolvedModule, src: NamedSource<String>) -> (TypedModule, DiagBag) {
    check_module_with_imports(resolved, src, &ImportedTypes::default())
}

/// Like [`check_module_with_src`], but with the type signatures of imported
/// modules' public items available for cross-module call checking.
pub fn check_module_with_imports(
    resolved: &ResolvedModule,
    src: NamedSource<String>,
    imported: &ImportedTypes,
) -> (TypedModule, DiagBag) {
    let mut c = Checker::new(resolved, src, imported);
    c.collect_decls();
    c.check_items();
    let typed = TypedModule {
        resolved: resolved.clone(),
        expr_tys: std::mem::take(&mut c.expr_tys),
        instantiations: std::mem::take(&mut c.instantiations),
        operator_sigs: std::mem::take(&mut c.operator_sigs),
        operator_locals: std::mem::take(&mut c.operator_locals),
    };
    (typed, c.diags)
}

// ---------------------------------------------------------------------------
// Checker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TableInfo {
    fields: Vec<(String, Ty)>,
    key_ty: Ty,
    value_ty: Ty,
}

/// An enum variant's payload as a scheme template over the enum's type params.
#[derive(Debug, Clone)]
enum PayloadSty {
    Unit,
    Tuple(Vec<STy>),
    Record(Vec<(String, STy)>),
}

#[derive(Debug, Clone)]
struct EnumInfo {
    params: Vec<String>,
    /// Variant name → payload, plus declaration order for diagnostics.
    variants: HashMap<String, PayloadSty>,
    order: Vec<String>,
}

#[derive(Debug, Clone)]
struct AliasInfo {
    params: Vec<String>,
    ty: Type,
    valid: bool,
}

/// What kind of body encloses the current expression (drives `?` legality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetCtx {
    Operator(EffectLevel),
    Lambda,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coll {
    Vector,
    Set,
    Bag,
}

struct Checker<'a> {
    resolved: &'a ResolvedModule,
    src: NamedSource<String>,
    diags: DiagBag,
    tables: HashMap<String, TableInfo>,
    enums: HashMap<String, EnumInfo>,
    variant_enum: HashMap<String, String>,
    aliases: HashMap<String, AliasInfo>,
    consts: HashMap<String, Ty>,
    ops: HashMap<String, Scheme>,
    op_levels: HashMap<String, EffectLevel>,
    /// Schemes of imported public operators (cross-module calls).
    imported_ops: HashMap<String, Scheme>,
    /// Local scopes; shadowing allowed, innermost scope last.
    env: Vec<HashMap<String, Ty>>,
    /// Return type + context of each enclosing operator/lambda body
    /// (innermost last), for `?` legality (§4.6).
    body_rets: Vec<(Ty, RetCtx)>,
    /// Type parameters of the operator currently being checked.
    cur_generics: HashSet<String>,
    /// Local-binding accumulator for the operator under checking.
    in_operator: bool,
    locals_acc: Vec<(String, Ty)>,
    alias_depth: usize,
    expr_tys: HashMap<Span, Ty>,
    instantiations: HashMap<Span, Vec<(String, Ty)>>,
    operator_sigs: HashMap<String, OperatorSig>,
    operator_locals: HashMap<String, Vec<(String, Ty)>>,
}

impl<'a> Checker<'a> {
    fn new(resolved: &'a ResolvedModule, src: NamedSource<String>, imported: &ImportedTypes) -> Self {
        // Pre-build schemes for imported operators; signatures containing
        // error types are dropped (the dependency already reported them).
        let mut imported_ops = HashMap::new();
        for (name, is) in &imported.ops {
            let params: Option<Vec<STy>> = is.sig.params.iter().map(|(_, t)| sty_of_ty(t)).collect();
            let ret = sty_of_ty(&is.sig.ret);
            if let (Some(params), Some(ret)) = (params, ret) {
                imported_ops.insert(
                    name.clone(),
                    Scheme {
                        tparams: is.type_params.clone(),
                        param_names: is.sig.params.iter().map(|(n, _)| n.clone()).collect(),
                        params,
                        ret,
                        ord: Vec::new(),
                        hash: Vec::new(),
                    },
                );
            }
        }
        Checker {
            resolved,
            src,
            diags: DiagBag::new(),
            tables: HashMap::new(),
            enums: HashMap::new(),
            variant_enum: HashMap::new(),
            aliases: HashMap::new(),
            consts: imported.consts.clone(),
            ops: HashMap::new(),
            op_levels: HashMap::new(),
            imported_ops,
            env: Vec::new(),
            body_rets: Vec::new(),
            cur_generics: HashSet::new(),
            in_operator: false,
            locals_acc: Vec::new(),
            alias_depth: 0,
            expr_tys: HashMap::new(),
            instantiations: HashMap::new(),
            operator_sigs: HashMap::new(),
            operator_locals: HashMap::new(),
        }
    }

    fn err(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_error(CqlError::new(self.src.clone(), span, message, help));
    }

    fn record(&mut self, e: &Expr, ty: &Ty) {
        self.expr_tys.insert(e.span, ty.clone());
    }

    fn mismatch(&mut self, span: Span, expected: &Ty, actual: &Ty) {
        if self.ty_eq(expected, actual) {
            return;
        }
        self.err(span, format!("type mismatch: expected `{}`, found `{}`", expected, actual), None);
    }

    // ---- declaration collection ----------------------------------------------

    fn collect_decls(&mut self) {
        let no_generics = HashSet::new();
        // Tables first: every other declaration may reference row types.
        for item in &self.resolved.module.items {
            if let Item::Table(t) = item {
                self.collect_table(t, &no_generics);
            }
        }
        for item in &self.resolved.module.items {
            if let Item::Table(t) = item {
                self.check_fks(t);
            }
        }
        for item in &self.resolved.module.items {
            match item {
                Item::TypeAlias(t) => self.collect_alias(t),
                Item::Enum(e) => self.collect_enum(e),
                _ => {}
            }
        }
        for item in &self.resolved.module.items {
            if let Item::Const(c) = item {
                let ty = self.ty_of(&c.ty, &no_generics);
                self.consts.insert(c.name.node.clone(), ty);
            }
        }
        for item in &self.resolved.module.items {
            if let Item::Operator(o) = item {
                self.collect_operator(o);
            }
        }
        for item in &self.resolved.module.items {
            if let Item::Index(i) = item {
                self.check_index(i);
            }
        }
    }

    fn collect_table(&mut self, t: &TableDecl, no_generics: &HashSet<String>) {
        let mut fields: Vec<(String, Ty)> = Vec::new();
        for (fname, fty) in &t.fields {
            fields.push((fname.node.clone(), self.ty_of(fty, no_generics)));
        }
        let pk: Vec<String> = t.pk.iter().map(|p| p.node.clone()).collect();
        let mut pk_tys: Vec<Ty> = Vec::new();
        let mut pk_ok = true;
        for pk_col in &t.pk {
            match fields.iter().find(|(n, _)| n == &pk_col.node) {
                None => {
                    self.err(
                        pk_col.span,
                        format!("primary key field `{}` is not a field of table `{}`", pk_col.node, t.name.node),
                        None,
                    );
                    pk_ok = false;
                }
                Some((_, fty)) => {
                    if !self.is_key_atom(fty) {
                        self.err(
                            pk_col.span,
                            format!(
                                "primary key field `{}` has type `{}`, which is not a valid key type",
                                pk_col.node, fty
                            ),
                            Some("key fields must be bool, int, decimal, string, or date (§2.3)".to_string()),
                        );
                        pk_ok = false;
                    }
                    pk_tys.push(fty.clone());
                }
            }
        }
        let key_ty = if !pk_ok {
            Ty::Error
        } else if pk_tys.len() == 1 {
            pk_tys.pop().unwrap()
        } else {
            Ty::Tuple(pk_tys)
        };
        let value_ty = Ty::Record(
            fields.iter().filter(|(n, _)| !pk.contains(n)).cloned().collect(),
        );
        self.tables.insert(
            t.name.node.clone(),
            TableInfo { fields, key_ty, value_ty },
        );
    }

    /// Valid key atoms (§2.3): hashable + ord base types; composite keys are
    /// tuples of these, assembled from multiple pk fields.
    fn is_key_atom(&self, ty: &Ty) -> bool {
        matches!(
            ty,
            Ty::Bool | Ty::Int | Ty::Decimal(_) | Ty::String | Ty::Date | Ty::Error
        )
    }

    fn check_fks(&mut self, t: &TableDecl) {
        for fk in &t.fks {
            let Some(rt) = self.tables.get(&fk.references.node).cloned() else {
                continue; // resolve already reported the missing table
            };
            let ref_comps: Vec<Ty> = match &rt.key_ty {
                Ty::Tuple(ts) => ts.clone(),
                other => vec![other.clone()],
            };
            if fk.cols.len() != ref_comps.len() {
                self.err(
                    fk.references.span,
                    format!(
                        "foreign key on `{}` has {} column(s) but table `{}` has a {}-column primary key",
                        t.name.node,
                        fk.cols.len(),
                        fk.references.node,
                        ref_comps.len()
                    ),
                    None,
                );
            }
            let own = self.tables.get(&t.name.node).cloned();
            for (col, comp) in fk.cols.iter().zip(ref_comps.iter()) {
                let own_ty = own
                    .as_ref()
                    .and_then(|o| o.fields.iter().find(|(n, _)| n == &col.node).map(|(_, t)| t.clone()));
                match own_ty {
                    None => self.err(
                        col.span,
                        format!("foreign key column `{}` is not a field of table `{}`", col.node, t.name.node),
                        None,
                    ),
                    Some(cty) => {
                        let opt = Ty::Option(Box::new(comp.clone()));
                        if !self.ty_eq(&cty, comp) && !self.ty_eq(&cty, &opt) {
                            self.err(
                                col.span,
                                format!(
                                    "foreign key column `{}` has type `{}`, expected `{}` or `option<{}>`",
                                    col.node, cty, comp, comp
                                ),
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    fn check_index(&mut self, i: &IndexDecl) {
        let Some(t) = self.tables.get(&i.table.node).cloned() else {
            return; // resolve already reported
        };
        for col in &i.cols {
            if !t.fields.iter().any(|(n, _)| n == &col.node) {
                self.err(
                    col.span,
                    format!("index column `{}` is not a field of table `{}`", col.node, i.table.node),
                    None,
                );
            }
        }
    }

    fn collect_alias(&mut self, t: &TypeAliasDecl) {
        let generics: HashSet<String> = t.params.iter().map(|p| p.node.clone()).collect();
        let before = self.diags.error_count();
        let _ = self.ty_of(&t.ty, &generics);
        let valid = self.diags.error_count() == before;
        self.aliases.insert(
            t.name.node.clone(),
            AliasInfo {
                params: t.params.iter().map(|p| p.node.clone()).collect(),
                ty: t.ty.clone(),
                valid,
            },
        );
    }

    fn collect_enum(&mut self, e: &EnumDecl) {
        let generics: HashSet<String> = e.params.iter().map(|p| p.node.clone()).collect();
        let mut variants = HashMap::new();
        let mut order = Vec::new();
        for v in &e.variants {
            let payload = match &v.payload {
                VariantPayload::None => PayloadSty::Unit,
                VariantPayload::Tuple(ts) => {
                    PayloadSty::Tuple(ts.iter().map(|t| self.sty_of(t, &generics)).collect())
                }
                VariantPayload::Record(fields) => PayloadSty::Record(
                    fields
                        .iter()
                        .map(|(n, t)| (n.node.clone(), self.sty_of(t, &generics)))
                        .collect(),
                ),
            };
            variants.insert(v.name.node.clone(), payload);
            order.push(v.name.node.clone());
            self.variant_enum.insert(v.name.node.clone(), e.name.node.clone());
        }
        self.enums.insert(
            e.name.node.clone(),
            EnumInfo {
                params: e.params.iter().map(|p| p.node.clone()).collect(),
                variants,
                order,
            },
        );
    }

    fn collect_operator(&mut self, o: &OperatorDecl) {
        let generics: HashSet<String> = o.type_params.iter().map(|p| p.node.clone()).collect();
        let params: Vec<STy> = o.params.iter().map(|p| self.sty_of(&p.ty, &generics)).collect();
        let ret = self.sty_of(&o.ret, &generics);
        let sc = Scheme {
            tparams: o.type_params.iter().map(|p| p.node.clone()).collect(),
            param_names: o.params.iter().map(|p| p.name.node.clone()).collect(),
            params,
            ret,
            ord: vec![],
            hash: vec![],
        };
        self.ops.insert(o.name.node.clone(), sc);
        self.op_levels.insert(o.name.node.clone(), o.level);
        // Concrete (Var-carrying) signature for the typed-module summary.
        let sig_params: Vec<(String, Ty)> = o
            .params
            .iter()
            .map(|p| (p.name.node.clone(), self.ty_of(&p.ty, &generics)))
            .collect();
        let sig_ret = self.ty_of(&o.ret, &generics);
        self.operator_sigs.insert(
            o.name.node.clone(),
            OperatorSig { params: sig_params, ret: sig_ret },
        );
    }

    // ---- item checking ---------------------------------------------------------

    fn check_items(&mut self) {
        for item in &self.resolved.module.items.clone() {
            match item {
                Item::Const(c) => {
                    let ty = self.consts.get(&c.name.node).cloned().unwrap_or(Ty::Error);
                    self.body_rets.push((ty.clone(), RetCtx::Other));
                    self.check(&c.value, &ty);
                    self.body_rets.pop();
                }
                Item::Operator(o) => self.check_operator(o),
                Item::Invariant(inv) => {
                    self.body_rets.push((Ty::Bool, RetCtx::Other));
                    self.check(&inv.body, &Ty::Bool);
                    self.body_rets.pop();
                }
                Item::Test(t) => {
                    for stmt in &t.stmts {
                        match stmt {
                            TestStmt::Fixture { table, rows } => {
                                let expected = Ty::Vector(Box::new(Ty::Row(table.node.clone())));
                                self.body_rets.push((expected.clone(), RetCtx::Other));
                                self.check(rows, &expected);
                                self.body_rets.pop();
                            }
                            TestStmt::Expect { lhs, rhs } => {
                                self.body_rets.push((Ty::Error, RetCtx::Other));
                                let l = self.infer(lhs);
                                self.check(rhs, &l);
                                self.body_rets.pop();
                            }
                        }
                    }
                }
                Item::Property(p) => self.check_temporal(&p.body),
                _ => {}
            }
        }
    }

    fn check_temporal(&mut self, t: &TemporalExpr) {
        match t {
            TemporalExpr::Always(inner) | TemporalExpr::Eventually(inner) => self.check_temporal(inner),
            TemporalExpr::LeadsTo { lhs, rhs } | TemporalExpr::Until { lhs, rhs } => {
                self.check_temporal(lhs);
                self.check_temporal(rhs);
            }
            TemporalExpr::Primed(e) | TemporalExpr::State(e) => {
                self.body_rets.push((Ty::Bool, RetCtx::Other));
                self.check(e, &Ty::Bool);
                self.body_rets.pop();
            }
        }
    }

    fn check_operator(&mut self, o: &OperatorDecl) {
        self.cur_generics = o.type_params.iter().map(|p| p.node.clone()).collect();
        let generics = self.cur_generics.clone();
        let ret_ty = self.ty_of(&o.ret, &generics);
        if o.level == EffectLevel::Action
            && !self.ty_eq(&ret_ty, &Ty::Set(Box::new(Ty::WriteOp)))
        {
            self.err(
                o.ret.span,
                format!("an `action` must return `set<write_op>`, found `{}`", ret_ty),
                None,
            );
        }
        let Some(body) = &o.body else { return };
        self.in_operator = true;
        self.locals_acc = Vec::new();
        self.env.push(HashMap::new());
        for p in &o.params {
            let ty = self.ty_of(&p.ty, &generics);
            self.bind(&p.name, ty);
        }
        self.body_rets.push((ret_ty.clone(), RetCtx::Operator(o.level)));
        self.check(body, &ret_ty);
        self.body_rets.pop();
        self.env.pop();
        self.in_operator = false;
        self.operator_locals
            .insert(o.name.node.clone(), std::mem::take(&mut self.locals_acc));
        self.cur_generics = HashSet::new();
    }

    // ---- locals -----------------------------------------------------------------

    fn bind(&mut self, name: &Ident, ty: Ty) {
        if let Some(scope) = self.env.last_mut() {
            scope.insert(name.node.clone(), ty.clone());
        }
        if self.in_operator {
            self.locals_acc.push((name.node.clone(), ty));
        }
    }

    fn lookup_local(&self, name: &str) -> Option<Ty> {
        self.env.iter().rev().find_map(|s| s.get(name)).cloned()
    }
}

// ---------------------------------------------------------------------------
// Type elaboration: surface Type -> Ty / STy
// ---------------------------------------------------------------------------

impl<'a> Checker<'a> {
    /// Elaborate a surface type into a checking type. `generics` holds the
    /// in-scope type parameters (mapped to [`Ty::Var`]). Table names become
    /// [`Ty::Row`], `key t`/`value t` expand to the derived types (§2.2),
    /// aliases expand, and `set`/`map` key constraints are verified.
    fn ty_of(&mut self, ty: &Type, generics: &HashSet<String>) -> Ty {
        match &ty.kind {
            TypeKind::Bool => Ty::Bool,
            TypeKind::Int => Ty::Int,
            TypeKind::Float => Ty::Float,
            TypeKind::Decimal(p) => Ty::Decimal(*p),
            TypeKind::String => Ty::String,
            TypeKind::Date => Ty::Date,
            TypeKind::Option(inner) => Ty::Option(Box::new(self.ty_of(inner, generics))),
            TypeKind::Vector(inner) => Ty::Vector(Box::new(self.ty_of(inner, generics))),
            TypeKind::Set(inner) => {
                let t = self.ty_of(inner, generics);
                self.require_hashable(&t, inner.span, "set elements");
                Ty::Set(Box::new(t))
            }
            TypeKind::Bag(inner) => {
                let t = self.ty_of(inner, generics);
                self.require_eq(&t, inner.span, "bag elements");
                Ty::Bag(Box::new(t))
            }
            TypeKind::Map(k, v) => {
                let kt = self.ty_of(k, generics);
                let vt = self.ty_of(v, generics);
                self.require_hashable(&kt, k.span, "map keys");
                Ty::Map(Box::new(kt), Box::new(vt))
            }
            TypeKind::Tuple(items) => {
                Ty::Tuple(items.iter().map(|t| self.ty_of(t, generics)).collect())
            }
            TypeKind::Record(fields) => Ty::Record(
                fields
                    .iter()
                    .map(|(n, t)| (n.node.clone(), self.ty_of(t, generics)))
                    .collect(),
            ),
            TypeKind::Fun(a, b) => Ty::Fun(
                Box::new(self.ty_of(a, generics)),
                Box::new(self.ty_of(b, generics)),
            ),
            TypeKind::Table(_, _) => {
                self.err(
                    ty.span,
                    "`table<...>` is not a first-class type",
                    Some("tables are only accessible via `read`/`lookup` (§4.3)".to_string()),
                );
                Ty::Error
            }
            TypeKind::Key(t) => match self.tables.get(&t.node).cloned() {
                Some(info) => info.key_ty,
                None => Ty::Error, // resolve already reported
            },
            TypeKind::Value(t) => match self.tables.get(&t.node).cloned() {
                Some(info) => info.value_ty,
                None => Ty::Error,
            },
            TypeKind::Named { name, args } => {
                if generics.contains(&name.node) {
                    return Ty::Var(name.node.clone());
                }
                if name.node == "write_op" {
                    return Ty::WriteOp;
                }
                if self.tables.contains_key(&name.node) {
                    return Ty::Row(name.node.clone());
                }
                if self.enums.contains_key(&name.node) {
                    let args = args.iter().map(|a| self.ty_of(a, generics)).collect();
                    return Ty::Enum { name: name.node.clone(), args };
                }
                if self.aliases.contains_key(&name.node) {
                    return self.expand_alias(name, args, generics);
                }
                Ty::Error // unknown names were reported by resolve
            }
        }
    }

    /// Elaborate a surface type into a scheme type (for generic signatures).
    fn sty_of(&mut self, ty: &Type, generics: &HashSet<String>) -> STy {
        match &ty.kind {
            TypeKind::Bool => STy::Bool,
            TypeKind::Int => STy::Int,
            TypeKind::Float => STy::Float,
            TypeKind::Decimal(p) => STy::Decimal(*p),
            TypeKind::String => STy::String,
            TypeKind::Date => STy::Date,
            TypeKind::Option(inner) => STy::Option(Box::new(self.sty_of(inner, generics))),
            TypeKind::Vector(inner) => STy::Vector(Box::new(self.sty_of(inner, generics))),
            TypeKind::Set(inner) => STy::Set(Box::new(self.sty_of(inner, generics))),
            TypeKind::Bag(inner) => STy::Bag(Box::new(self.sty_of(inner, generics))),
            TypeKind::Map(k, v) => STy::Map(
                Box::new(self.sty_of(k, generics)),
                Box::new(self.sty_of(v, generics)),
            ),
            TypeKind::Tuple(items) => {
                STy::Tuple(items.iter().map(|t| self.sty_of(t, generics)).collect())
            }
            TypeKind::Record(fields) => STy::Record(
                fields
                    .iter()
                    .map(|(n, t)| (n.node.clone(), self.sty_of(t, generics)))
                    .collect(),
            ),
            TypeKind::Fun(a, b) => STy::Fun(
                Box::new(self.sty_of(a, generics)),
                Box::new(self.sty_of(b, generics)),
            ),
            TypeKind::Table(_, _) => STy::Var("@invalid".to_string()),
            TypeKind::Key(_) | TypeKind::Value(_) => {
                // Derived types in generic signatures: elaborate concretely
                // and embed (they never mention the type parameters).
                let no_generics = HashSet::new();
                let t = self.ty_of(ty, &no_generics);
                self.sty_from_ty(t)
            }
            TypeKind::Named { name, args } => {
                if generics.contains(&name.node) {
                    return STy::Var(name.node.clone());
                }
                if name.node == "write_op" {
                    return STy::WriteOp;
                }
                if self.tables.contains_key(&name.node) {
                    return STy::Row(name.node.clone());
                }
                if self.enums.contains_key(&name.node) {
                    let args = args.iter().map(|a| self.sty_of(a, generics)).collect();
                    return STy::Enum { name: name.node.clone(), args };
                }
                if self.aliases.contains_key(&name.node) {
                    let no_generics = HashSet::new();
                    let t = self.expand_alias(name, args, &no_generics);
                    return self.sty_from_ty(t);
                }
                STy::Var("@invalid".to_string())
            }
        }
    }

    /// Embed a concrete type into a scheme (lossless except for `Ty::Var`,
    /// which cannot appear here since aliases are expanded without generics).
    fn sty_from_ty(&self, ty: Ty) -> STy {
        match ty {
            Ty::Bool => STy::Bool,
            Ty::Int => STy::Int,
            Ty::Float => STy::Float,
            Ty::Decimal(p) => STy::Decimal(p),
            Ty::String => STy::String,
            Ty::Date => STy::Date,
            Ty::Option(t) => STy::Option(Box::new(self.sty_from_ty(*t))),
            Ty::Vector(t) => STy::Vector(Box::new(self.sty_from_ty(*t))),
            Ty::Set(t) => STy::Set(Box::new(self.sty_from_ty(*t))),
            Ty::Bag(t) => STy::Bag(Box::new(self.sty_from_ty(*t))),
            Ty::Map(k, v) => STy::Map(
                Box::new(self.sty_from_ty(*k)),
                Box::new(self.sty_from_ty(*v)),
            ),
            Ty::Tuple(ts) => STy::Tuple(ts.into_iter().map(|t| self.sty_from_ty(t)).collect()),
            Ty::Record(fs) => {
                STy::Record(fs.into_iter().map(|(n, t)| (n, self.sty_from_ty(t))).collect())
            }
            Ty::Fun(a, b) => STy::Fun(
                Box::new(self.sty_from_ty(*a)),
                Box::new(self.sty_from_ty(*b)),
            ),
            Ty::Enum { name, args } => STy::Enum {
                name,
                args: args.into_iter().map(|t| self.sty_from_ty(t)).collect(),
            },
            Ty::Row(n) => STy::Row(n),
            Ty::WriteOp => STy::WriteOp,
            Ty::Var(n) => STy::Var(n),
            Ty::Error => STy::Var("@invalid".to_string()),
        }
    }

    /// Expand a type alias: substitute the surface argument types for the
    /// alias's parameters, then elaborate. Cyclic aliases are cut off by a
    /// depth bound.
    fn expand_alias(&mut self, name: &Ident, args: &[Type], generics: &HashSet<String>) -> Ty {
        let Some(info) = self.aliases.get(&name.node).cloned() else {
            return Ty::Error;
        };
        if !info.valid {
            return Ty::Error; // reported at the alias declaration
        }
        if self.alias_depth > 32 {
            self.err(name.span, format!("type alias `{}` expands cyclically", name.node), None);
            return Ty::Error;
        }
        if args.len() != info.params.len() {
            self.err(
                name.span,
                format!(
                    "type alias `{}` expects {} type argument(s), got {}",
                    name.node,
                    info.params.len(),
                    args.len()
                ),
                None,
            );
            return Ty::Error;
        }
        let subst: HashMap<String, Type> = info
            .params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let expanded = subst_type(&info.ty, &subst);
        self.alias_depth += 1;
        let t = self.ty_of(&expanded, generics);
        self.alias_depth -= 1;
        t
    }

    // ---- type predicates --------------------------------------------------------

    /// Structural equality of checking types. [`Ty::Error`] unifies with
    /// anything (cascade suppression); [`Ty::Row`] compares by its record
    /// expansion; record field order is insignificant.
    fn ty_eq(&self, a: &Ty, b: &Ty) -> bool {
        match (a, b) {
            (Ty::Error, _) | (_, Ty::Error) => true,
            (Ty::Bool, Ty::Bool)
            | (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Float)
            | (Ty::String, Ty::String)
            | (Ty::Date, Ty::Date)
            | (Ty::WriteOp, Ty::WriteOp) => true,
            (Ty::Decimal(x), Ty::Decimal(y)) => x == y,
            (Ty::Var(x), Ty::Var(y)) => x == y,
            (Ty::Row(x), Ty::Row(y)) => x == y,
            (Ty::Row(n), Ty::Record(_)) => {
                let fs = self.row_fields(n).map(Ty::Record);
                fs.is_some_and(|r| self.records_eq(&r, b))
            }
            (Ty::Record(_), Ty::Row(n)) => {
                let fs = self.row_fields(n).map(Ty::Record);
                fs.is_some_and(|r| self.records_eq(a, &r))
            }
            (Ty::Option(x), Ty::Option(y))
            | (Ty::Vector(x), Ty::Vector(y))
            | (Ty::Set(x), Ty::Set(y))
            | (Ty::Bag(x), Ty::Bag(y)) => self.ty_eq(x, y),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) => self.ty_eq(k1, k2) && self.ty_eq(v1, v2),
            (Ty::Fun(a1, b1), Ty::Fun(a2, b2)) => self.ty_eq(a1, a2) && self.ty_eq(b1, b2),
            (Ty::Tuple(xs), Ty::Tuple(ys)) => {
                xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| self.ty_eq(x, y))
            }
            (Ty::Record(_), Ty::Record(_)) => self.records_eq(a, b),
            (Ty::Enum { name: n1, args: a1 }, Ty::Enum { name: n2, args: a2 }) => {
                n1 == n2
                    && a1.len() == a2.len()
                    && a1.iter().zip(a2).all(|(x, y)| self.ty_eq(x, y))
            }
            _ => false,
        }
    }

    fn records_eq(&self, a: &Ty, b: &Ty) -> bool {
        let (Ty::Record(fa), Ty::Record(fb)) = (a, b) else {
            return false;
        };
        fa.len() == fb.len()
            && fa.iter().all(|(n, t)| {
                fb.iter()
                    .find(|(m, _)| m == n)
                    .is_some_and(|(_, u)| self.ty_eq(t, u))
            })
    }

    fn row_fields(&self, name: &str) -> Option<Vec<(String, Ty)>> {
        self.tables.get(name).map(|t| t.fields.clone())
    }

    /// The field list of a record or row type.
    fn fields_of(&self, ty: &Ty) -> Option<Vec<(String, Ty)>> {
        match ty {
            Ty::Record(fs) => Some(fs.clone()),
            Ty::Row(n) => self.row_fields(n),
            _ => None,
        }
    }

    /// Expand a row type to its record form; other types pass through.
    fn deref_row(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Row(n) => match self.row_fields(n) {
                Some(fs) => Ty::Record(fs),
                None => ty.clone(),
            },
            other => other.clone(),
        }
    }

    /// Hashable (§2.3): everything except `float`, function types, and types
    /// containing them. Row types are hashable (rows are identified by their
    /// primary key). Rigid type parameters are assumed hashable — generic
    /// functions are unconstrained (§3.4) and instantiation is local.
    fn is_hashable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Float | Ty::Fun(..) => false,
            Ty::Bool
            | Ty::Int
            | Ty::Decimal(_)
            | Ty::String
            | Ty::Date
            | Ty::WriteOp
            | Ty::Row(_)
            | Ty::Var(_)
            | Ty::Error => true,
            Ty::Option(t) | Ty::Vector(t) | Ty::Set(t) | Ty::Bag(t) => self.is_hashable(t),
            Ty::Map(k, v) => self.is_hashable(k) && self.is_hashable(v),
            Ty::Tuple(ts) => ts.iter().all(|t| self.is_hashable(t)),
            Ty::Record(fs) => fs.iter().all(|(_, t)| self.is_hashable(t)),
            Ty::Enum { name, args } => {
                let Some(info) = self.enums.get(name) else { return true };
                let subst: Subst = info.params.iter().cloned().zip(args.iter().cloned()).collect();
                info.variants.values().all(|p| match p {
                    PayloadSty::Unit => true,
                    PayloadSty::Tuple(ts) => ts
                        .iter()
                        .all(|s| self.subst_sty(s, &subst).is_none_or(|t| self.is_hashable(&t))),
                    PayloadSty::Record(fs) => fs
                        .iter()
                        .all(|(_, s)| self.subst_sty(s, &subst).is_none_or(|t| self.is_hashable(&t))),
                })
            }
        }
    }

    /// Ordered (§2.3): bool, int, float, decimal, string, date, and tuples of
    /// ordered types.
    fn is_ord(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Bool | Ty::Int | Ty::Float | Ty::Decimal(_) | Ty::String | Ty::Date => true,
            Ty::Var(_) | Ty::Error => true,
            Ty::Tuple(ts) => ts.iter().all(|t| self.is_ord(t)),
            _ => false,
        }
    }

    /// Supports structural equality (§2.3): every first-order data type;
    /// function types are excluded. `float` compares per IEEE 754.
    fn is_eq(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Fun(..) => false,
            Ty::Bool
            | Ty::Int
            | Ty::Float
            | Ty::Decimal(_)
            | Ty::String
            | Ty::Date
            | Ty::WriteOp
            | Ty::Var(_)
            | Ty::Error => true,
            Ty::Row(n) => self
                .row_fields(n)
                .is_none_or(|fs| fs.iter().all(|(_, t)| self.is_eq(t))),
            Ty::Option(t) | Ty::Vector(t) | Ty::Set(t) | Ty::Bag(t) => self.is_eq(t),
            Ty::Map(k, v) => self.is_eq(k) && self.is_eq(v),
            Ty::Tuple(ts) => ts.iter().all(|t| self.is_eq(t)),
            Ty::Record(fs) => fs.iter().all(|(_, t)| self.is_eq(t)),
            Ty::Enum { name, args } => {
                let Some(info) = self.enums.get(name) else { return true };
                let subst: Subst = info.params.iter().cloned().zip(args.iter().cloned()).collect();
                info.variants.values().all(|p| match p {
                    PayloadSty::Unit => true,
                    PayloadSty::Tuple(ts) => ts
                        .iter()
                        .all(|s| self.subst_sty(s, &subst).is_none_or(|t| self.is_eq(&t))),
                    PayloadSty::Record(fs) => fs
                        .iter()
                        .all(|(_, s)| self.subst_sty(s, &subst).is_none_or(|t| self.is_eq(&t))),
                })
            }
        }
    }

    fn require_hashable(&mut self, ty: &Ty, span: Span, what: &str) {
        if !self.is_hashable(ty) {
            self.err(
                span,
                format!("{} requires a hashable type, but `{}` is not hashable", what, ty),
                Some(
                    "hashable types exclude `float`, function types, and any type containing them (§2.3)"
                        .to_string(),
                ),
            );
        }
    }

    fn require_ord(&mut self, ty: &Ty, span: Span, what: &str) {
        if !self.is_ord(ty) {
            self.err(
                span,
                format!("{} requires an ordered type, but `{}` is not ordered", what, ty),
                Some(
                    "ordered types: bool, int, float, decimal, string, date, and tuples of those (§2.3)"
                        .to_string(),
                ),
            );
        }
    }

    fn require_eq(&mut self, ty: &Ty, span: Span, what: &str) {
        if !self.is_eq(ty) {
            self.err(
                span,
                format!("`{}` does not support equality (required for {})", ty, what),
                Some("function types are not comparable (§2.3)".to_string()),
            );
        }
    }

    // ---- scheme instantiation -----------------------------------------------------

    /// Substitute bound variables in a scheme; `None` if any variable or
    /// decimal meta-variable remains unbound.
    fn subst_sty(&self, sty: &STy, subst: &Subst) -> Option<Ty> {
        match sty {
            STy::Bool => Some(Ty::Bool),
            STy::Int => Some(Ty::Int),
            STy::Float => Some(Ty::Float),
            STy::String => Some(Ty::String),
            STy::Date => Some(Ty::Date),
            STy::Decimal(p) => Some(Ty::Decimal(*p)),
            STy::WriteOp => Some(Ty::WriteOp),
            STy::Row(n) => Some(Ty::Row(n.clone())),
            STy::Var(n) => subst.get(n).cloned(),
            STy::DecMeta(n) => subst.get(&dec_meta_key(n)).cloned(),
            STy::Option(t) => Some(Ty::Option(Box::new(self.subst_sty(t, subst)?))),
            STy::Vector(t) => Some(Ty::Vector(Box::new(self.subst_sty(t, subst)?))),
            STy::Set(t) => Some(Ty::Set(Box::new(self.subst_sty(t, subst)?))),
            STy::Bag(t) => Some(Ty::Bag(Box::new(self.subst_sty(t, subst)?))),
            STy::SetOrBag(t) => self.subst_sty(t, subst),
            STy::Map(k, v) => Some(Ty::Map(
                Box::new(self.subst_sty(k, subst)?),
                Box::new(self.subst_sty(v, subst)?),
            )),
            STy::Tuple(ts) => ts
                .iter()
                .map(|t| self.subst_sty(t, subst))
                .collect::<Option<Vec<_>>>()
                .map(Ty::Tuple),
            STy::Record(fs) => fs
                .iter()
                .map(|(n, t)| self.subst_sty(t, subst).map(|t| (n.clone(), t)))
                .collect::<Option<Vec<_>>>()
                .map(Ty::Record),
            STy::Fun(a, b) => Some(Ty::Fun(
                Box::new(self.subst_sty(a, subst)?),
                Box::new(self.subst_sty(b, subst)?),
            )),
            STy::Enum { name, args } => Some(Ty::Enum {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|t| self.subst_sty(t, subst))
                    .collect::<Option<Vec<_>>>()?,
            }),
        }
    }

    /// Like [`Checker::subst_sty`] but defaults unbound decimal
    /// meta-variables to the unbounded `decimal` (their scale comes from the
    /// parsed value at runtime; §2.1).
    fn subst_sty_default_dec(&self, sty: &STy, subst: &Subst) -> Ty {
        match sty {
            STy::DecMeta(n) => subst
                .get(&dec_meta_key(n))
                .cloned()
                .unwrap_or(Ty::Decimal(None)),
            STy::Option(t) => Ty::Option(Box::new(self.subst_sty_default_dec(t, subst))),
            STy::Vector(t) => Ty::Vector(Box::new(self.subst_sty_default_dec(t, subst))),
            STy::Set(t) => Ty::Set(Box::new(self.subst_sty_default_dec(t, subst))),
            STy::Bag(t) => Ty::Bag(Box::new(self.subst_sty_default_dec(t, subst))),
            STy::SetOrBag(t) => self.subst_sty_default_dec(t, subst),
            STy::Map(k, v) => Ty::Map(
                Box::new(self.subst_sty_default_dec(k, subst)),
                Box::new(self.subst_sty_default_dec(v, subst)),
            ),
            STy::Tuple(ts) => {
                Ty::Tuple(ts.iter().map(|t| self.subst_sty_default_dec(t, subst)).collect())
            }
            STy::Record(fs) => Ty::Record(
                fs.iter()
                    .map(|(n, t)| (n.clone(), self.subst_sty_default_dec(t, subst)))
                    .collect(),
            ),
            STy::Fun(a, b) => Ty::Fun(
                Box::new(self.subst_sty_default_dec(a, subst)),
                Box::new(self.subst_sty_default_dec(b, subst)),
            ),
            STy::Enum { name, args } => Ty::Enum {
                name: name.clone(),
                args: args.iter().map(|t| self.subst_sty_default_dec(t, subst)).collect(),
            },
            other => self.subst_sty(other, subst).unwrap_or(Ty::Error),
        }
    }

    /// Match a scheme against an actual type, binding variables on success.
    /// Pure (no diagnostics); rolls nothing back — callers snapshot `subst`.
    fn matches(&self, sty: &STy, ty: &Ty, subst: &mut Subst) -> bool {
        if matches!(ty, Ty::Error) {
            return true;
        }
        match sty {
            STy::Var(n) => match subst.get(n) {
                Some(bound) => self.ty_eq(bound, ty),
                None => {
                    subst.insert(n.clone(), ty.clone());
                    true
                }
            },
            STy::DecMeta(n) => match ty {
                Ty::Decimal(_) => {
                    let key = dec_meta_key(n);
                    match subst.get(&key) {
                        Some(bound) => self.ty_eq(bound, ty),
                        None => {
                            subst.insert(key, ty.clone());
                            true
                        }
                    }
                }
                _ => false,
            },
            STy::SetOrBag(inner) => match ty {
                Ty::Set(t) | Ty::Bag(t) => self.matches(inner, t, subst),
                _ => false,
            },
            STy::Row(n) => matches!(ty, Ty::Row(m) if m == n),
            // A record scheme also accepts a row type with matching fields.
            STy::Record(_) => match ty {
                Ty::Row(_) => self.matches(sty, &self.deref_row(ty), subst),
                Ty::Record(fs) => {
                    let STy::Record(sfs) = sty else { unreachable!() };
                    sfs.len() == fs.len()
                        && sfs.iter().all(|(n, s)| {
                            fs.iter()
                                .find(|(m, _)| m == n)
                                .is_some_and(|(_, t)| self.matches(s, t, subst))
                        })
                }
                _ => false,
            },
            STy::Bool => matches!(ty, Ty::Bool),
            STy::Int => matches!(ty, Ty::Int),
            STy::Float => matches!(ty, Ty::Float),
            STy::String => matches!(ty, Ty::String),
            STy::Date => matches!(ty, Ty::Date),
            STy::Decimal(p) => matches!(ty, Ty::Decimal(q) if q == p),
            STy::WriteOp => matches!(ty, Ty::WriteOp),
            STy::Option(s) => match ty {
                Ty::Option(t) => self.matches(s, t, subst),
                _ => false,
            },
            STy::Vector(s) => match ty {
                Ty::Vector(t) => self.matches(s, t, subst),
                _ => false,
            },
            STy::Set(s) => match ty {
                Ty::Set(t) => self.matches(s, t, subst),
                _ => false,
            },
            STy::Bag(s) => match ty {
                Ty::Bag(t) => self.matches(s, t, subst),
                _ => false,
            },
            STy::Map(sk, sv) => match ty {
                Ty::Map(k, v) => self.matches(sk, k, subst) && self.matches(sv, v, subst),
                _ => false,
            },
            STy::Tuple(ss) => match ty {
                Ty::Tuple(ts) => {
                    ss.len() == ts.len()
                        && ss.iter().zip(ts).all(|(s, t)| self.matches(s, t, subst))
                }
                _ => false,
            },
            STy::Fun(sa, sb) => match ty {
                Ty::Fun(a, b) => self.matches(sa, a, subst) && self.matches(sb, b, subst),
                _ => false,
            },
            STy::Enum { name, args } => match ty {
                Ty::Enum { name: n2, args: a2 } => {
                    name == n2
                        && args.len() == a2.len()
                        && args.iter().zip(a2).all(|(s, t)| self.matches(s, t, subst))
                }
                _ => false,
            },
        }
    }

    /// Render a scheme with the current substitution, for error messages.
    fn sty_str(&self, sty: &STy, subst: &Subst) -> String {
        match self.subst_sty(sty, subst) {
            Some(t) => t.to_string(),
            None => match sty {
                STy::Var(n) => n.clone(),
                STy::DecMeta(_) => "decimal".to_string(),
                STy::Option(t) => format!("option<{}>", self.sty_str(t, subst)),
                STy::Vector(t) => format!("vector<{}>", self.sty_str(t, subst)),
                STy::Set(t) => format!("set<{}>", self.sty_str(t, subst)),
                STy::Bag(t) => format!("bag<{}>", self.sty_str(t, subst)),
                STy::SetOrBag(t) => format!("set<{}> or bag<{}>", self.sty_str(t, subst), self.sty_str(t, subst)),
                STy::Map(k, v) => format!("map<{}, {}>", self.sty_str(k, subst), self.sty_str(v, subst)),
                STy::Tuple(ts) => {
                    let inner: Vec<String> = ts.iter().map(|t| self.sty_str(t, subst)).collect();
                    format!("({})", inner.join(", "))
                }
                STy::Record(fs) => {
                    let inner: Vec<String> = fs
                        .iter()
                        .map(|(n, t)| format!("{}: {}", n, self.sty_str(t, subst)))
                        .collect();
                    format!("{{ {} }}", inner.join(", "))
                }
                STy::Fun(a, b) => format!("{} -> {}", self.sty_str(a, subst), self.sty_str(b, subst)),
                STy::Enum { name, args } => {
                    let inner: Vec<String> = args.iter().map(|t| self.sty_str(t, subst)).collect();
                    format!("{}<{}>", name, inner.join(", "))
                }
                other => self.subst_sty(other, subst).map_or("?".to_string(), |t| t.to_string()),
            },
        }
    }
}

/// Substitute surface types for a type alias's parameters.
fn subst_type(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    let kind = match &ty.kind {
        TypeKind::Named { name, args } if args.is_empty() && subst.contains_key(&name.node) => {
            return subst[&name.node].clone();
        }
        TypeKind::Named { name, args } => TypeKind::Named {
            name: name.clone(),
            args: args.iter().map(|a| subst_type(a, subst)).collect(),
        },
        TypeKind::Bool => TypeKind::Bool,
        TypeKind::Int => TypeKind::Int,
        TypeKind::Float => TypeKind::Float,
        TypeKind::Decimal(p) => TypeKind::Decimal(*p),
        TypeKind::String => TypeKind::String,
        TypeKind::Date => TypeKind::Date,
        TypeKind::Key(t) => TypeKind::Key(t.clone()),
        TypeKind::Value(t) => TypeKind::Value(t.clone()),
        TypeKind::Option(t) => TypeKind::Option(Box::new(subst_type(t, subst))),
        TypeKind::Vector(t) => TypeKind::Vector(Box::new(subst_type(t, subst))),
        TypeKind::Set(t) => TypeKind::Set(Box::new(subst_type(t, subst))),
        TypeKind::Bag(t) => TypeKind::Bag(Box::new(subst_type(t, subst))),
        TypeKind::Map(k, v) => {
            TypeKind::Map(Box::new(subst_type(k, subst)), Box::new(subst_type(v, subst)))
        }
        TypeKind::Table(k, v) => {
            TypeKind::Table(Box::new(subst_type(k, subst)), Box::new(subst_type(v, subst)))
        }
        TypeKind::Tuple(ts) => TypeKind::Tuple(ts.iter().map(|t| subst_type(t, subst)).collect()),
        TypeKind::Fun(a, b) => TypeKind::Fun(
            Box::new(subst_type(a, subst)),
            Box::new(subst_type(b, subst)),
        ),
        TypeKind::Record(fs) => TypeKind::Record(
            fs.iter().map(|(n, t)| (n.clone(), subst_type(t, subst))).collect(),
        ),
    };
    Type::new(kind, ty.span)
}

// ---------------------------------------------------------------------------
// Bidirectional checking
// ---------------------------------------------------------------------------

impl<'a> Checker<'a> {
    /// Check `e` against an expected type (§2.5: literals, `none`, empty
    /// collections, record literals, lambdas, branches and blocks absorb the
    /// expected type; everything else infers and compares).
    fn check(&mut self, e: &Expr, expected: &Ty) -> Ty {
        if matches!(expected, Ty::Error) {
            return self.infer(e);
        }
        let ty = match &e.kind {
            ExprKind::OptionNone => match expected {
                Ty::Option(_) => expected.clone(),
                _ => {
                    self.err(
                        e.span,
                        format!("type mismatch: expected `{}`, found `none`", expected),
                        None,
                    );
                    expected.clone()
                }
            },
            ExprKind::OptionSome(inner) => match expected {
                Ty::Option(t) => {
                    let t = t.clone();
                    self.check(inner, &t);
                    expected.clone()
                }
                _ => {
                    let actual = Ty::Option(Box::new(self.infer(inner)));
                    self.mismatch(e.span, expected, &actual);
                    expected.clone()
                }
            },
            ExprKind::Vector(items) => self.check_seq(e, items, expected, Coll::Vector),
            ExprKind::SetLiteral(items) => self.check_seq(e, items, expected, Coll::Set),
            ExprKind::BagLiteral(items) => self.check_seq(e, items, expected, Coll::Bag),
            ExprKind::MapLit(entries) => match expected {
                Ty::Map(k, v) => {
                    let (k, v) = (k.clone(), v.clone());
                    for (ke, ve) in entries {
                        self.check(ke, &k);
                        self.check(ve, &v);
                    }
                    expected.clone()
                }
                _ => {
                    let actual = self.infer_map_lit(entries, e.span);
                    self.mismatch(e.span, expected, &actual);
                    expected.clone()
                }
            },
            ExprKind::Tuple(items) => match expected {
                Ty::Tuple(ts) if ts.len() == items.len() => {
                    let ts = ts.clone();
                    for (item, t) in items.iter().zip(ts) {
                        self.check(item, &t);
                    }
                    expected.clone()
                }
                _ => {
                    let actual = Ty::Tuple(items.iter().map(|i| self.infer(i)).collect());
                    self.mismatch(e.span, expected, &actual);
                    expected.clone()
                }
            },
            ExprKind::RecordLit { fields } => self.check_record_lit(e, fields, expected),
            ExprKind::Lambda(l) => match expected.clone() {
                Ty::Fun(p, r) => self.check_lambda(e, l, &p, &r),
                other => {
                    let actual = self.infer_lambda(e, l);
                    self.mismatch(e.span, &other, &actual);
                    other
                }
            },
            ExprKind::If { cond, then_br, else_br } => {
                self.check(cond, &Ty::Bool);
                self.check(then_br, expected);
                self.check(else_br, expected);
                expected.clone()
            }
            ExprKind::Match { scrutinee, arms } => {
                self.infer_match(e, scrutinee, arms, Some(expected))
            }
            ExprKind::Block { lets, tail } => {
                self.env.push(HashMap::new());
                for l in lets {
                    self.let_stmt(l);
                }
                let t = self.check(tail, expected);
                self.env.pop();
                t
            }
            ExprKind::Let { pat, value, body } => {
                let v = self.infer(value);
                self.env.push(HashMap::new());
                self.bind_pat(pat, &v);
                let t = self.check(body, expected);
                self.env.pop();
                t
            }
            ExprKind::Call(_) => self.call(e, Some(expected)),
            ExprKind::App { func, args } => self.infer_app(e, func, args, Some(expected)),
            ExprKind::MethodCall { recv, name, args } => {
                self.method_call(e, recv, name, args, Some(expected))
            }
            ExprKind::EnumConstruct { name, args } => {
                let actual = self.enum_construct(e, name, args, Some(expected));
                self.mismatch(e.span, expected, &actual);
                expected.clone()
            }
            _ => {
                let actual = self.infer(e);
                self.mismatch(e.span, expected, &actual);
                expected.clone()
            }
        };
        self.record(e, &ty);
        ty
    }

    /// Synthesize the type of `e`.
    fn infer(&mut self, e: &Expr) -> Ty {
        let ty = match &e.kind {
            ExprKind::Lit(l) => self.infer_lit(l, e.span),
            ExprKind::Var(name) => self.infer_var(e, name),
            ExprKind::Block { lets, tail } => {
                self.env.push(HashMap::new());
                for l in lets {
                    self.let_stmt(l);
                }
                let t = self.infer(tail);
                self.env.pop();
                t
            }
            ExprKind::Let { pat, value, body } => {
                let v = self.infer(value);
                self.env.push(HashMap::new());
                self.bind_pat(pat, &v);
                let t = self.infer(body);
                self.env.pop();
                t
            }
            ExprKind::Lambda(l) => self.infer_lambda(e, l),
            ExprKind::App { func, args } => self.infer_app(e, func, args, None),
            ExprKind::Call(_) => self.call(e, None),
            ExprKind::Match { scrutinee, arms } => self.infer_match(e, scrutinee, arms, None),
            ExprKind::If { cond, then_br, else_br } => {
                self.check(cond, &Ty::Bool);
                let t = self.infer(then_br);
                self.check(else_br, &t);
                t
            }
            ExprKind::Try(inner) => self.infer_try(inner, e.span),
            ExprKind::RecordLit { fields } => Ty::Record(
                fields
                    .iter()
                    .map(|f| (f.name.node.clone(), self.infer(&f.value)))
                    .collect(),
            ),
            ExprKind::RecordUpd { base, fields } => self.infer_record_upd(base, fields),
            ExprKind::Tuple(items) => Ty::Tuple(items.iter().map(|i| self.infer(i)).collect()),
            ExprKind::Vector(items) => self.infer_seq(items, Coll::Vector, e.span),
            ExprKind::SetLiteral(items) => self.infer_seq(items, Coll::Set, e.span),
            ExprKind::BagLiteral(items) => self.infer_seq(items, Coll::Bag, e.span),
            ExprKind::SetFilter { pat, source, pred } => {
                let elem = self.gen_source(source);
                self.env.push(HashMap::new());
                self.bind_pat(pat, &elem);
                self.check(pred, &Ty::Bool);
                self.env.pop();
                self.require_hashable(&elem, source.span, "set comprehension elements");
                Ty::Set(Box::new(elem))
            }
            ExprKind::SetMap { elem, gens } => {
                let et = self.comprehension_elem(elem, gens);
                self.require_hashable(&et, elem.span, "set comprehension elements");
                Ty::Set(Box::new(et))
            }
            ExprKind::BagMap { elem, gens } => {
                let et = self.comprehension_elem(elem, gens);
                self.require_eq(&et, elem.span, "bag comprehension elements");
                Ty::Bag(Box::new(et))
            }
            ExprKind::MapLit(entries) => self.infer_map_lit(entries, e.span),
            ExprKind::OptionSome(inner) => Ty::Option(Box::new(self.infer(inner))),
            ExprKind::OptionNone => {
                self.err(
                    e.span,
                    "cannot infer the type of `none`",
                    Some("add a type annotation, e.g. `let x: option<int> == none`".to_string()),
                );
                Ty::Error
            }
            ExprKind::StrInterp(parts) => self.infer_str_interp(parts, e.span),
            ExprKind::Quantifier { gens, body, .. } => {
                self.env.push(HashMap::new());
                for g in gens {
                    let t = self.gen_source(&g.source);
                    self.bind_pat(&g.pat, &t);
                }
                self.check(body, &Ty::Bool);
                self.env.pop();
                Ty::Bool
            }
            ExprKind::Cast { expr, ty } => self.infer_cast(expr, ty, e.span),
            ExprKind::BinOp { op, lhs, rhs } => self.infer_binop(*op, lhs, rhs, e.span),
            ExprKind::UnOp { op, operand } => self.infer_unop(*op, operand, e.span),
            ExprKind::Primed(inner) => self.infer(inner),
            ExprKind::Field { base, name } => self.infer_field(base, name, e.span),
            ExprKind::TupleProj { base, index } => self.infer_proj(base, *index, e.span),
            ExprKind::MethodCall { recv, name, args } => self.method_call(e, recv, name, args, None),
            ExprKind::ReadPrim { table, predicate } => {
                let row = Ty::Row(table.node.clone());
                let pred_ty = Ty::Fun(Box::new(row.clone()), Box::new(Ty::Bool));
                self.check(predicate, &pred_ty);
                Ty::Set(Box::new(row))
            }
            ExprKind::WriteCon(w) => self.infer_write_con(w),
            ExprKind::EnumConstruct { name, args } => self.enum_construct(e, name, args, None),
        };
        self.record(e, &ty);
        ty
    }

    fn let_stmt(&mut self, l: &LetStmt) {
        match &l.ty {
            Some(ann) => {
                let generics = self.cur_generics.clone();
                let t = self.ty_of(ann, &generics);
                self.check(&l.value, &t);
                self.bind_pat(&l.pat, &t);
            }
            None => {
                let v = self.infer(&l.value);
                self.bind_pat(&l.pat, &v);
            }
        }
    }

    // ---- literals ----------------------------------------------------------------

    fn infer_lit(&mut self, l: &Literal, span: Span) -> Ty {
        match l {
            Literal::Bool(_) => Ty::Bool,
            Literal::Int(_) => Ty::Int,
            Literal::Float(_) => Ty::Float,
            Literal::Str(_) => Ty::String,
            Literal::Date { year, month, day } => {
                self.check_date_lit(*year, *month, *day, span);
                Ty::Date
            }
            Literal::Decimal { repr, precision } => {
                self.check_decimal_lit(repr, *precision, span);
                Ty::Decimal(*precision)
            }
        }
    }

    /// Validate a decimal literal against its declared precision (§2.1:
    /// significant digits ≤ m, fractional digits ≤ n; no implicit rounding).
    fn check_decimal_lit(&mut self, repr: &str, precision: Option<(u32, u32)>, span: Span) {
        let Some((m, n)) = precision else { return };
        let clean: String = repr.chars().filter(|c| *c != '_').collect();
        let (int_part, frac_part) = match clean.split_once('.') {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (clean.clone(), String::new()),
        };
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            return; // malformed literal: the parser's responsibility
        }
        let scale = frac_part.len() as u32;
        let digits = format!("{}{}", int_part, frac_part);
        let sig = digits.trim_start_matches('0').len().max(1) as u32;
        if sig > m {
            self.err(
                span,
                format!(
                    "decimal literal `{}` has {} significant digit(s) but `decimal({}, {})` allows at most {}",
                    repr, sig, m, n, m
                ),
                Some("choose a wider precision; CQL never rounds implicitly (§2.1)".to_string()),
            );
        }
        if scale > n {
            self.err(
                span,
                format!(
                    "decimal literal `{}` has {} fractional digit(s) but `decimal({}, {})` allows at most {}",
                    repr, scale, m, n, n
                ),
                Some("choose a larger scale; CQL never rounds implicitly (§2.1)".to_string()),
            );
        }
    }

    fn check_date_lit(&mut self, year: i32, month: u8, day: u8, span: Span) {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => 0,
        };
        if days_in_month == 0 || day == 0 || day > days_in_month {
            self.err(
                span,
                format!("invalid date literal `{}-{:02}-{:02}`", year, month, day),
                None,
            );
        }
    }

    // ---- variables & calls ----------------------------------------------------

    fn infer_var(&mut self, e: &Expr, name: &Ident) -> Ty {
        let ty = match self.resolved.resolved.vars.get(&name.span) {
            Some(VarRes::Local) => self.lookup_local(&name.node).unwrap_or(Ty::Error),
            Some(VarRes::Const) => self.consts.get(&name.node).cloned().unwrap_or(Ty::Error),
            Some(VarRes::Function) => {
                let sc = self
                    .ops
                    .get(&name.node)
                    .or_else(|| self.imported_ops.get(&name.node))
                    .cloned();
                match sc {
                    Some(sc) => self.fn_value_ty(&name.node, name.span, &sc),
                    None => Ty::Error,
                }
            }
            Some(VarRes::StdLibFn) => match stdlib_scheme(&name.node) {
                Some(sc) => self.fn_value_ty(&name.node, name.span, &sc),
                None => {
                    // `length`/`map` are overloaded; dispatch needs a call.
                    self.err(
                        name.span,
                        format!("overloaded function `{}` cannot be used as a value", name.node),
                        Some("call it directly so the overload can be selected".to_string()),
                    );
                    Ty::Error
                }
            },
            Some(VarRes::TableSugar) => Ty::Set(Box::new(Ty::Row(name.node.clone()))),
            None => Ty::Error, // resolve failed for this name
        };
        self.record(e, &ty);
        ty
    }

    /// The type of a non-generic function used as a first-class value.
    fn fn_value_ty(&mut self, name: &str, span: Span, sc: &Scheme) -> Ty {
        if !sc.tparams.is_empty() {
            self.err(
                span,
                format!("generic function `{}` used as a value needs its type arguments", name),
                Some("call it with turbofish (`f::<T>(x)`) instead of passing it".to_string()),
            );
            return Ty::Error;
        }
        let subst = Subst::new();
        let mut params: Vec<Ty> = sc
            .params
            .iter()
            .map(|p| self.subst_sty(p, &subst).unwrap_or(Ty::Error))
            .collect();
        let param = if params.len() == 1 {
            params.pop().unwrap()
        } else {
            Ty::Tuple(params)
        };
        let ret = self.subst_sty(&sc.ret, &subst).unwrap_or(Ty::Error);
        Ty::Fun(Box::new(param), Box::new(ret))
    }

    /// A named call, dispatched via the resolution side table.
    fn call(&mut self, e: &Expr, expected: Option<&Ty>) -> Ty {
        let ExprKind::Call(call) = &e.kind else { unreachable!() };
        let span = call.name.span;
        let callee = self.resolved.resolved.callee.get(&span).cloned();
        let ty = match callee {
            None => Ty::Error, // resolve failed for this call
            Some(Callee::LookupPrim) => {
                self.reject_turbofish(call, "lookup");
                self.lookup_call(call)
            }
            Some(Callee::LocalValue) => {
                self.reject_turbofish(call, &call.name.node);
                let fty = self.lookup_local(&call.name.node).unwrap_or(Ty::Error);
                self.apply_fun_ty(&fty, &call.args, e.span)
            }
            Some(Callee::GlobalValue) => {
                self.reject_turbofish(call, &call.name.node);
                let fty = self.consts.get(&call.name.node).cloned().unwrap_or(Ty::Error);
                self.apply_fun_ty(&fty, &call.args, e.span)
            }
            Some(Callee::Operator { name, module_local: true, .. }) => {
                let Some(sc) = self.ops.get(&name).cloned() else {
                    return Ty::Error;
                };
                let ordered = self.order_args(&sc.param_names, &call.args);
                self.instantiate_scheme(span, &name, sc, call.type_args.as_ref(), ordered, expected)
            }
            Some(Callee::Operator { module_local: false, name, .. }) => {
                // Imported operator: its public signature comes from the
                // dependency's compiled interface (multi-module projects).
                match self.imported_ops.get(&name).cloned() {
                    Some(sc) => {
                        let ordered = self.order_args(&sc.param_names, &call.args);
                        self.instantiate_scheme(span, &name, sc, call.type_args.as_ref(), ordered, expected)
                    }
                    None => {
                        // Signature unavailable (dependency failed to
                        // compile): check the arguments for their own sake.
                        for a in &call.args {
                            self.infer(&a.value);
                        }
                        Ty::Error
                    }
                }
            }
            Some(Callee::StdLib { name }) => {
                let Some(sc) = self.stdlib_dispatch(&name, &call.args) else {
                    for a in &call.args {
                        self.infer(&a.value);
                    }
                    return Ty::Error;
                };
                let ordered = self.order_args(&sc.param_names, &call.args);
                self.instantiate_scheme(span, &name, sc, call.type_args.as_ref(), ordered, expected)
            }
        };
        self.record(e, &ty);
        ty
    }

    fn reject_turbofish(&mut self, call: &Call, name: &str) {
        if call.type_args.is_some() {
            self.err(
                call.name.span,
                format!("`{}` is not generic and takes no type arguments", name),
                None,
            );
        }
    }

    /// `lookup(t, k) -> option<value t>` (§4.3).
    fn lookup_call(&mut self, call: &Call) -> Ty {
        let table_name = match call.args.first().map(|a| &a.value.kind) {
            Some(ExprKind::Var(t)) => t.node.clone(),
            _ => return Ty::Error, // resolve already reported
        };
        let Some(info) = self.tables.get(&table_name).cloned() else {
            return Ty::Error;
        };
        if let Some(key) = call.args.get(1) {
            self.check(&key.value, &info.key_ty);
        }
        Ty::Option(Box::new(info.value_ty))
    }

    /// Order call arguments by parameter name (named-argument coverage and
    /// position rules were already validated by resolve). Returns
    /// `(parameter index, argument)` pairs.
    fn order_args<'e>(&mut self, param_names: &[String], args: &'e [Arg]) -> Vec<(usize, &'e Expr)> {
        let mut out = Vec::new();
        for (i, a) in args.iter().enumerate() {
            match &a.name {
                None => {
                    if i < param_names.len() {
                        out.push((i, &a.value));
                    }
                }
                Some(n) => {
                    if let Some(p) = param_names.iter().position(|pn| pn == &n.node) {
                        out.push((p, &a.value));
                    }
                }
            }
        }
        out
    }

    /// Apply a function type to positional arguments (multi-argument
    /// functions take a tuple, §4.2).
    fn apply_fun_ty(&mut self, fty: &Ty, args: &[Arg], span: Span) -> Ty {
        let Ty::Fun(p, r) = fty else {
            if !matches!(fty, Ty::Error) {
                self.err(
                    span,
                    format!("expression of type `{}` is not callable", fty),
                    None,
                );
            }
            return Ty::Error;
        };
        let (p, r) = ((**p).clone(), (**r).clone());
        if args.len() == 1 {
            self.check(&args[0].value, &p);
        } else {
            match &p {
                Ty::Tuple(ts) if ts.len() == args.len() => {
                    let ts = ts.clone();
                    for (a, t) in args.iter().zip(ts) {
                        self.check(&a.value, &t);
                    }
                }
                _ => self.err(
                    span,
                    format!("function expects {} argument(s), got {}", tuple_len(&p), args.len()),
                    None,
                ),
            }
        }
        r
    }

    /// A call of an arbitrary function expression (§4.2).
    fn infer_app(&mut self, e: &Expr, func: &Expr, args: &[Arg], expected: Option<&Ty>) -> Ty {
        // A plain `function`/stdlib name in function position instantiates
        // its generic signature from the arguments, like a named call.
        if let ExprKind::Var(name) = &func.kind {
            let res = self.resolved.resolved.vars.get(&name.span).cloned();
            let sc = match res {
                Some(VarRes::Function) => self
                    .ops
                    .get(&name.node)
                    .or_else(|| self.imported_ops.get(&name.node))
                    .cloned(),
                Some(VarRes::StdLibFn) => stdlib_scheme(&name.node),
                _ => None,
            };
            if let Some(sc) = sc {
                let ordered: Vec<(usize, &Expr)> =
                    args.iter().enumerate().map(|(i, a)| (i, &a.value)).collect();
                let ty = self.instantiate_scheme(name.span, &name.node, sc, None, ordered, expected);
                self.record(e, &ty);
                return ty;
            }
        }
        let fty = self.infer(func);
        let ty = self.apply_fun_ty(&fty, args, e.span);
        self.record(e, &ty);
        ty
    }

    /// Select the `length`/`map` overload by the first argument's type
    /// (appendix B: the only two same-name dispatches).
    fn stdlib_dispatch(&mut self, name: &str, args: &[Arg]) -> Option<Scheme> {
        if name != "length" && name != "map" {
            return stdlib_scheme(name);
        }
        let first_ty = args.first().map(|a| self.infer(&a.value)).unwrap_or(Ty::Error);
        let span = args.first().map(|a| a.value.span).unwrap_or(Span::new_dummy());
        match (name, &first_ty) {
            (_, Ty::Error) => None,
            ("length", Ty::String) => Some(length_string_scheme()),
            ("length", Ty::Vector(_)) => Some(length_vector_scheme()),
            ("map", Ty::Vector(_)) => Some(map_vector_scheme()),
            ("map", Ty::Option(_)) => Some(map_option_scheme()),
            _ => {
                self.err(
                    span,
                    format!("no overload of `{}` applies to type `{}`", name, first_ty),
                    Some(match name {
                        "length" => "`length` is defined for `string` and `vector<T>`".to_string(),
                        _ => "`map` is defined for `vector<T>` and `option<T>`".to_string(),
                    }),
                );
                None
            }
        }
    }

    /// Instantiate a generic scheme at a call site (§2.5): left-to-right
    /// matching of argument types against parameter schemes binds the type
    /// parameters; turbofish arguments pre-bind and are validated; the
    /// expected result type (when known) may bind remaining parameters.
    fn instantiate_scheme(
        &mut self,
        span: Span,
        name: &str,
        sc: Scheme,
        type_args: Option<&Vec<Type>>,
        args: Vec<(usize, &Expr)>,
        expected: Option<&Ty>,
    ) -> Ty {
        let mut subst: Subst = Subst::new();
        if let Some(tas) = type_args {
            if sc.tparams.is_empty() {
                self.err(
                    span,
                    format!("`{}` is not generic and takes no type arguments", name),
                    None,
                );
            } else if tas.len() != sc.tparams.len() {
                self.err(
                    span,
                    format!(
                        "`{}` expects {} type argument(s), got {}",
                        name,
                        sc.tparams.len(),
                        tas.len()
                    ),
                    None,
                );
            } else {
                let generics = self.cur_generics.clone();
                for (tp, ta) in sc.tparams.iter().zip(tas) {
                    let t = self.ty_of(ta, &generics);
                    subst.insert(tp.clone(), t);
                }
            }
        }
        for (i, arg) in args {
            let Some(param) = sc.params.get(i).cloned() else {
                continue; // arity errors were reported by resolve
            };
            self.check_arg_scheme(&param, arg, &mut subst);
        }
        // Bind remaining parameters from the expected result type.
        if let Some(exp) = expected {
            let ret = sc.ret.clone();
            let mut trial = subst.clone();
            if self.matches(&ret, exp, &mut trial) {
                subst = trial;
            }
        }
        // Constraints: hashable for set/map key positions (derived), plus the
        // scheme's explicit `hash`/`ord` lists.
        let mut hash_vars: Vec<String> = sc.hash.iter().map(|s| s.to_string()).collect();
        for p in sc.params.iter().chain(std::iter::once(&sc.ret)) {
            collect_set_key_vars(p, &mut hash_vars);
        }
        hash_vars.sort();
        hash_vars.dedup();
        for v in &hash_vars {
            if let Some(t) = subst.get(v).cloned() {
                self.require_hashable(&t, span, &format!("type parameter `{}` of `{}`", v, name));
            }
        }
        for v in &sc.ord {
            if let Some(t) = subst.get(*v).cloned() {
                self.require_ord(&t, span, &format!("type parameter `{}` of `{}`", v, name));
            }
        }
        // Result.
        match self.subst_sty(&sc.ret, &subst) {
            Some(t) => {
                let inst: Vec<(String, Ty)> = sc
                    .tparams
                    .iter()
                    .filter_map(|p| subst.get(p).map(|t| (p.clone(), t.clone())))
                    .collect();
                if !inst.is_empty() {
                    self.instantiations.insert(span, inst);
                }
                t
            }
            None => {
                let unbound: Vec<&String> =
                    sc.tparams.iter().filter(|p| !subst.contains_key(*p)).collect();
                if unbound.is_empty() {
                    // Only a decimal meta-variable is undetermined; default it
                    // to the unbounded decimal (§2.1).
                    self.subst_sty_default_dec(&sc.ret, &subst)
                } else {
                    let names: Vec<String> = unbound.iter().map(|s| format!("`{}`", s)).collect();
                    self.err(
                        span,
                        format!(
                            "cannot infer type parameter(s) {} for `{}`",
                            names.join(", "),
                            name
                        ),
                        Some(format!("provide explicit type arguments: `{}::<...>(...)`", name)),
                    );
                    Ty::Error
                }
            }
        }
    }

    /// Check one argument against a parameter scheme. Lambdas absorb the
    /// (possibly still partially unbound) function scheme.
    fn check_arg_scheme(&mut self, param: &STy, arg: &Expr, subst: &mut Subst) {
        if let ExprKind::Lambda(l) = &arg.kind {
            if let STy::Fun(p, r) = param {
                let (p, r) = ((**p).clone(), (**r).clone());
                self.lambda_scheme(arg, l, &p, &r, subst);
                return;
            }
        }
        let actual = self.infer(arg);
        self.match_report(param, &actual, subst, arg.span);
    }

    /// Match a scheme against an inferred argument type, reporting one
    /// mismatch error (with rollback) on failure.
    fn match_report(&mut self, param: &STy, actual: &Ty, subst: &mut Subst, span: Span) {
        let mut trial = subst.clone();
        if self.matches(param, actual, &mut trial) {
            *subst = trial;
        } else {
            self.err(
                span,
                format!(
                    "argument type mismatch: expected `{}`, found `{}`",
                    self.sty_str(param, subst),
                    actual
                ),
                None,
            );
        }
    }

    // ---- lambdas ------------------------------------------------------------------

    /// A lambda against a fully known function type (§2.5: parameter types
    /// from the expected signature; explicit annotations win and are checked
    /// for consistency).
    fn check_lambda(&mut self, e: &Expr, l: &Lambda, p_ty: &Ty, r_ty: &Ty) -> Ty {
        let n = l.params.len();
        let comps: Vec<Option<Ty>> = if n == 1 {
            vec![Some(p_ty.clone())]
        } else {
            match p_ty {
                Ty::Tuple(ts) if ts.len() == n => ts.iter().map(|t| Some(t.clone())).collect(),
                _ => {
                    self.err(
                        e.span,
                        format!(
                            "lambda takes {} parameter(s) but a function of type `{}` was expected",
                            n, p_ty
                        ),
                        None,
                    );
                    vec![None; n]
                }
            }
        };
        let generics = self.cur_generics.clone();
        let mut param_tys: Vec<Ty> = Vec::new();
        for (i, p) in l.params.iter().enumerate() {
            let t = match &p.ty {
                Some(ann) => {
                    let t = self.ty_of(ann, &generics);
                    if let Some(comp) = &comps[i] {
                        self.mismatch(ann.span, comp, &t);
                    }
                    t
                }
                None => comps[i].clone().unwrap_or(Ty::Error),
            };
            param_tys.push(t);
        }
        self.env.push(HashMap::new());
        for (p, t) in l.params.iter().zip(&param_tys) {
            self.bind_pat(&p.pat, t);
        }
        let ret_ty = match &l.ret {
            Some(ann) => {
                let r = self.ty_of(ann, &generics);
                self.mismatch(ann.span, r_ty, &r);
                self.body_rets.push((r.clone(), RetCtx::Lambda));
                self.check(&l.body, &r);
                self.body_rets.pop();
                r
            }
            None => {
                self.body_rets.push((r_ty.clone(), RetCtx::Lambda));
                self.check(&l.body, r_ty);
                self.body_rets.pop();
                r_ty.clone()
            }
        };
        self.env.pop();
        let param = if param_tys.len() == 1 {
            param_tys.pop().unwrap()
        } else {
            Ty::Tuple(param_tys)
        };
        Ty::Fun(Box::new(param), Box::new(ret_ty))
    }

    /// A lambda against a scheme function type: annotations bind scheme
    /// variables; unannotated parameters take the substituted scheme
    /// components; an unbound result scheme is inferred from the body.
    fn lambda_scheme(&mut self, e: &Expr, l: &Lambda, p_sty: &STy, r_sty: &STy, subst: &mut Subst) {
        let n = l.params.len();
        let comps: Vec<Option<STy>> = if n == 1 {
            vec![Some(p_sty.clone())]
        } else {
            match p_sty {
                STy::Tuple(ts) if ts.len() == n => ts.iter().map(|t| Some(t.clone())).collect(),
                _ => {
                    self.err(
                        e.span,
                        format!(
                            "lambda takes {} parameter(s) but a `{}` argument was expected",
                            n,
                            self.sty_str(p_sty, subst)
                        ),
                        None,
                    );
                    vec![None; n]
                }
            }
        };
        let generics = self.cur_generics.clone();
        // Pass A: explicit annotations.
        let mut param_tys: Vec<Option<Ty>> = vec![None; n];
        for (i, p) in l.params.iter().enumerate() {
            if let Some(ann) = &p.ty {
                param_tys[i] = Some(self.ty_of(ann, &generics));
            }
        }
        if param_tys.iter().all(Option::is_some) {
            let pt = if n == 1 {
                param_tys[0].clone().unwrap()
            } else {
                Ty::Tuple(param_tys.iter().map(|t| t.clone().unwrap()).collect())
            };
            let mut trial = subst.clone();
            if self.matches(p_sty, &pt, &mut trial) {
                *subst = trial;
            } else {
                self.err(
                    e.span,
                    format!(
                        "lambda parameter type `{}` does not match expected `{}`",
                        pt,
                        self.sty_str(p_sty, subst)
                    ),
                    None,
                );
            }
        }
        // Pass B: unannotated parameters take the substituted components.
        for (i, p) in l.params.iter().enumerate() {
            if param_tys[i].is_none() {
                let t = comps[i].as_ref().and_then(|c| self.subst_sty(c, subst));
                match t {
                    Some(t) => param_tys[i] = Some(t),
                    None => {
                        self.err(
                            p.pat.span,
                            "cannot infer the type of this lambda parameter",
                            Some("add a type annotation (the type parameters are not yet determined)".to_string()),
                        );
                        param_tys[i] = Some(Ty::Error);
                    }
                }
            }
        }
        self.env.push(HashMap::new());
        for (p, t) in l.params.iter().zip(&param_tys) {
            self.bind_pat(&p.pat, t.as_ref().unwrap());
        }
        // Body / result.
        let ret_ty;
        match &l.ret {
            Some(ann) => {
                let r = self.ty_of(ann, &generics);
                let mut trial = subst.clone();
                if self.matches(r_sty, &r, &mut trial) {
                    *subst = trial;
                } else {
                    self.err(
                        ann.span,
                        format!(
                            "lambda return type `{}` does not match expected `{}`",
                            r,
                            self.sty_str(r_sty, subst)
                        ),
                        None,
                    );
                }
                self.body_rets.push((r.clone(), RetCtx::Lambda));
                self.check(&l.body, &r);
                self.body_rets.pop();
                ret_ty = r;
            }
            None => match self.subst_sty(r_sty, subst) {
                Some(r) => {
                    self.body_rets.push((r.clone(), RetCtx::Lambda));
                    self.check(&l.body, &r);
                    self.body_rets.pop();
                    ret_ty = r;
                }
                None => {
                    self.body_rets.push((Ty::Error, RetCtx::Lambda));
                    let t = self.infer(&l.body);
                    self.body_rets.pop();
                    let mut trial = subst.clone();
                    if self.matches(r_sty, &t, &mut trial) {
                        *subst = trial;
                    } else {
                        self.err(
                            l.body.span,
                            format!(
                                "lambda body type `{}` does not match expected `{}`",
                                t,
                                self.sty_str(r_sty, subst)
                            ),
                            None,
                        );
                    }
                    ret_ty = t;
                }
            },
        }
        self.env.pop();
        // Record the lambda's function type for later passes (CIR lowering
        // needs it to type the lifted function and its parameters).
        let param = if param_tys.len() == 1 {
            param_tys[0].clone().unwrap()
        } else {
            Ty::Tuple(param_tys.iter().map(|t| t.clone().unwrap()).collect())
        };
        self.record(e, &Ty::Fun(Box::new(param), Box::new(ret_ty)));
    }

    /// A lambda with no expected signature: every parameter must be
    /// annotated (§2.5); the result is the annotation or the inferred body.
    fn infer_lambda(&mut self, _e: &Expr, l: &Lambda) -> Ty {
        let generics = self.cur_generics.clone();
        let mut param_tys: Vec<Ty> = Vec::new();
        for p in &l.params {
            match &p.ty {
                Some(ann) => param_tys.push(self.ty_of(ann, &generics)),
                None => {
                    self.err(
                        p.pat.span,
                        "cannot infer the type of this lambda parameter",
                        Some("add a type annotation".to_string()),
                    );
                    param_tys.push(Ty::Error);
                }
            }
        }
        self.env.push(HashMap::new());
        for (p, t) in l.params.iter().zip(&param_tys) {
            self.bind_pat(&p.pat, t);
        }
        let ret_ty = match &l.ret {
            Some(ann) => {
                let r = self.ty_of(ann, &generics);
                self.body_rets.push((r.clone(), RetCtx::Lambda));
                self.check(&l.body, &r);
                self.body_rets.pop();
                r
            }
            None => {
                self.body_rets.push((Ty::Error, RetCtx::Lambda));
                let r = self.infer(&l.body);
                self.body_rets.pop();
                r
            }
        };
        self.env.pop();
        let param = if param_tys.len() == 1 {
            param_tys.pop().unwrap()
        } else {
            Ty::Tuple(param_tys)
        };
        Ty::Fun(Box::new(param), Box::new(ret_ty))
    }

    // ---- collections --------------------------------------------------------------

    fn infer_seq(&mut self, items: &[Expr], coll: Coll, span: Span) -> Ty {
        let (name, open) = match coll {
            Coll::Vector => ("vector", "[]"),
            Coll::Set => ("set", "set {}"),
            Coll::Bag => ("bag", "bag {}"),
        };
        if items.is_empty() {
            self.err(
                span,
                format!("cannot infer the type of an empty `{}`", open),
                Some(format!("add a type annotation, e.g. `let xs: {}<int> == {}`", name, open)),
            );
            return Ty::Error;
        }
        let first = self.infer(&items[0]);
        for item in &items[1..] {
            self.check(item, &first);
        }
        match coll {
            Coll::Vector => Ty::Vector(Box::new(first)),
            Coll::Set => {
                self.require_hashable(&first, span, "set elements");
                Ty::Set(Box::new(first))
            }
            Coll::Bag => {
                self.require_eq(&first, span, "bag elements");
                Ty::Bag(Box::new(first))
            }
        }
    }

    fn check_seq(&mut self, e: &Expr, items: &[Expr], expected: &Ty, coll: Coll) -> Ty {
        let elem = match (coll, expected) {
            (Coll::Vector, Ty::Vector(t)) | (Coll::Set, Ty::Set(t)) | (Coll::Bag, Ty::Bag(t)) => {
                Some(t.clone())
            }
            _ => None,
        };
        match elem {
            Some(t) => {
                for item in items {
                    self.check(item, &t);
                }
                expected.clone()
            }
            None => {
                let actual = self.infer_seq(items, coll, e.span);
                self.mismatch(e.span, expected, &actual);
                expected.clone()
            }
        }
    }

    fn infer_map_lit(&mut self, entries: &[(Expr, Expr)], span: Span) -> Ty {
        if entries.is_empty() {
            self.err(
                span,
                "cannot infer the type of an empty `map {}`",
                Some("add a type annotation, e.g. `let m: map<string, int> == map {}`".to_string()),
            );
            return Ty::Error;
        }
        let k0 = self.infer(&entries[0].0);
        let v0 = self.infer(&entries[0].1);
        for (ke, ve) in &entries[1..] {
            self.check(ke, &k0);
            self.check(ve, &v0);
        }
        self.require_hashable(&k0, entries[0].0.span, "map keys");
        Ty::Map(Box::new(k0), Box::new(v0))
    }

    /// The element type of a generator/quantifier source (§4.4.1: set, bag,
    /// vector, option, or the table-name sugar which infers as `set<row>`).
    fn gen_source(&mut self, source: &Expr) -> Ty {
        let t = self.infer(source);
        match t {
            Ty::Set(e) | Ty::Bag(e) | Ty::Vector(e) | Ty::Option(e) => *e,
            Ty::Error => Ty::Error,
            other => {
                self.err(
                    source.span,
                    format!(
                        "generator source must be a set, bag, vector, option, or table name, found `{}`",
                        other
                    ),
                    None,
                );
                Ty::Error
            }
        }
    }

    fn comprehension_elem(&mut self, elem: &Expr, gens: &[Generator]) -> Ty {
        self.env.push(HashMap::new());
        for g in gens {
            let t = self.gen_source(&g.source);
            self.bind_pat(&g.pat, &t);
        }
        let et = self.infer(elem);
        self.env.pop();
        et
    }

    // ---- records ----------------------------------------------------------------

    fn check_record_lit(&mut self, e: &Expr, fields: &[FieldInit], expected: &Ty) -> Ty {
        let Some(exp_fields) = self.fields_of(expected) else {
            let actual = Ty::Record(
                fields
                    .iter()
                    .map(|f| (f.name.node.clone(), self.infer(&f.value)))
                    .collect(),
            );
            self.mismatch(e.span, expected, &actual);
            return expected.clone();
        };
        for f in fields {
            match exp_fields.iter().find(|(n, _)| n == &f.name.node) {
                Some((_, fty)) => {
                    let fty = fty.clone();
                    self.check(&f.value, &fty);
                }
                None => {
                    self.err(
                        f.name.span,
                        format!("unknown field `{}` for type `{}`", f.name.node, expected),
                        None,
                    );
                    self.infer(&f.value);
                }
            }
        }
        let missing: Vec<&String> = exp_fields
            .iter()
            .map(|(n, _)| n)
            .filter(|n| !fields.iter().any(|f| &f.name.node == *n))
            .collect();
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|n| format!("`{}`", n)).collect();
            self.err(
                e.span,
                format!("record literal is missing field(s) {}", names.join(", ")),
                None,
            );
        }
        expected.clone()
    }

    /// `record { base with f: v, ... }`: base must be a record/row type, the
    /// overridden fields must exist with matching types, and the result is
    /// the base type (§4.1).
    fn infer_record_upd(&mut self, base: &Expr, fields: &[FieldInit]) -> Ty {
        let base_ty = self.infer(base);
        match self.fields_of(&base_ty) {
            None => {
                if !matches!(base_ty, Ty::Error) {
                    self.err(
                        base.span,
                        format!("record update on non-record type `{}`", base_ty),
                        None,
                    );
                }
                for f in fields {
                    self.infer(&f.value);
                }
                Ty::Error
            }
            Some(fs) => {
                for f in fields {
                    match fs.iter().find(|(n, _)| n == &f.name.node) {
                        Some((_, fty)) => {
                            let fty = fty.clone();
                            self.check(&f.value, &fty);
                        }
                        None => {
                            self.err(
                                f.name.span,
                                format!("type `{}` has no field `{}`", base_ty, f.name.node),
                                None,
                            );
                            self.infer(&f.value);
                        }
                    }
                }
                base_ty
            }
        }
    }

    fn infer_field(&mut self, base: &Expr, name: &Ident, span: Span) -> Ty {
        let base_ty = self.infer(base);
        match self.fields_of(&base_ty) {
            Some(fs) => match fs.iter().find(|(n, _)| n == &name.node) {
                Some((_, t)) => t.clone(),
                None => {
                    self.err(
                        span,
                        format!("type `{}` has no field `{}`", base_ty, name.node),
                        None,
                    );
                    Ty::Error
                }
            },
            None => {
                if !matches!(base_ty, Ty::Error) {
                    self.err(
                        base.span,
                        format!("field access on non-record type `{}`", base_ty),
                        None,
                    );
                }
                Ty::Error
            }
        }
    }

    fn infer_proj(&mut self, base: &Expr, index: u32, span: Span) -> Ty {
        let base_ty = self.infer(base);
        match &base_ty {
            Ty::Tuple(ts) => match ts.get(index as usize) {
                Some(t) => t.clone(),
                None => {
                    self.err(
                        span,
                        format!("tuple `{}` has no component `{}`", base_ty, index),
                        None,
                    );
                    Ty::Error
                }
            },
            Ty::Error => Ty::Error,
            other => {
                self.err(
                    base.span,
                    format!("component projection on non-tuple type `{}`", other),
                    None,
                );
                Ty::Error
            }
        }
    }

    // ---- method-call sugar ----------------------------------------------------------

    /// `recv.name(args)` (§4.1/A.3): a function-typed record field `name`
    /// wins (field call); otherwise dispatch as `name(recv, args)` to a
    /// module-level `function` (which shadows the stdlib) or a stdlib
    /// function, with `length`/`map` overloads selected by the receiver type.
    fn method_call(
        &mut self,
        e: &Expr,
        recv: &Expr,
        name: &Ident,
        args: &[Arg],
        expected: Option<&Ty>,
    ) -> Ty {
        for a in args {
            if let Some(n) = &a.name {
                self.err(
                    n.span,
                    "named arguments are not allowed in method calls",
                    None,
                );
            }
        }
        let recv_ty = self.infer(recv);
        let ty = (|| {
            if let Some(fs) = self.fields_of(&recv_ty) {
                if let Some((_, fty)) = fs.iter().find(|(n, _)| n == &name.node).cloned() {
                    if matches!(fty, Ty::Fun(..)) {
                        return self.apply_fun_ty(&fty, args, e.span);
                    }
                }
            }
            let mut all: Vec<(usize, &Expr)> = vec![(0, recv)];
            for (i, a) in args.iter().enumerate() {
                all.push((i + 1, &a.value));
            }
            // A module-level function shadows the stdlib (A.3).
            if self.op_levels.get(&name.node) == Some(&EffectLevel::Function) {
                let sc = self.ops.get(&name.node).cloned().unwrap();
                return self.instantiate_scheme(name.span, &name.node, sc, None, all, expected);
            }
            if stdlib_signature(&name.node).is_some() {
                let first = Arg { name: None, value: recv.clone() };
                let mut full_args = vec![first];
                full_args.extend(args.iter().cloned());
                match self.stdlib_dispatch(&name.node, &full_args) {
                    Some(sc) => {
                        return self.instantiate_scheme(name.span, &name.node, sc, None, all, expected);
                    }
                    None => return Ty::Error,
                }
            }
            // `m.get(k)` etc.: on a map receiver, `name` falls back to the
            // `map_<name>` stdlib family (§4.10).
            if matches!(recv_ty, Ty::Map(..)) {
                let prefixed = format!("map_{}", name.node);
                if stdlib_signature(&prefixed).is_some() {
                    let first = Arg { name: None, value: recv.clone() };
                    let mut full_args = vec![first];
                    full_args.extend(args.iter().cloned());
                    match self.stdlib_dispatch(&prefixed, &full_args) {
                        Some(sc) => {
                            return self
                                .instantiate_scheme(name.span, &prefixed, sc, None, all, expected);
                        }
                        None => return Ty::Error,
                    }
                }
            }
            self.err(
                name.span,
                format!("cannot resolve method call `{}` on type `{}`", name.node, recv_ty),
                Some("method calls dispatch to a function-typed field or a pure function `name(recv, ...)`".to_string()),
            );
            Ty::Error
        })();
        self.record(e, &ty);
        ty
    }

    // ---- operators --------------------------------------------------------------

    fn infer_binop(&mut self, op: BinOpKind, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        match op {
            BinOpKind::Add | BinOpKind::Sub | BinOpKind::Mul | BinOpKind::Div | BinOpKind::Mod => {
                let l = self.infer(lhs);
                let r = self.infer(rhs);
                let same = match (&l, &r) {
                    (Ty::Int, Ty::Int) => Some(Ty::Int),
                    (Ty::Float, Ty::Float) => Some(Ty::Float),
                    (Ty::Decimal(a), Ty::Decimal(b)) if a == b => Some(Ty::Decimal(*a)),
                    (Ty::Error, _) | (_, Ty::Error) => Some(Ty::Error),
                    _ => None,
                };
                match same {
                    Some(t) => t,
                    None => {
                        self.err(
                            span,
                            format!(
                                "operands of `{}` must have the same numeric type, found `{}` and `{}`",
                                binop_str(op),
                                l,
                                r
                            ),
                            Some("no implicit conversions; use `as` to convert (§2.4)".to_string()),
                        );
                        Ty::Error
                    }
                }
            }
            BinOpKind::Eq | BinOpKind::Ne => {
                let l = self.infer(lhs);
                self.check(rhs, &l);
                self.require_eq(&l, lhs.span, "operands of `=`/`/=`");
                Ty::Bool
            }
            BinOpKind::Lt | BinOpKind::Gt | BinOpKind::Le | BinOpKind::Ge => {
                let l = self.infer(lhs);
                self.check(rhs, &l);
                self.require_ord(&l, lhs.span, &format!("operands of `{}`", binop_str(op)));
                Ty::Bool
            }
            BinOpKind::And | BinOpKind::Or | BinOpKind::Impl => {
                self.check(lhs, &Ty::Bool);
                self.check(rhs, &Ty::Bool);
                Ty::Bool
            }
            BinOpKind::In => {
                let r = self.infer(rhs);
                let elem = match r {
                    Ty::Set(t) | Ty::Bag(t) | Ty::Vector(t) | Ty::Option(t) => *t,
                    Ty::Error => Ty::Error,
                    other => {
                        self.err(
                            rhs.span,
                            format!(
                                "right operand of `\\in` must be a set, bag, vector, or option, found `{}`",
                                other
                            ),
                            None,
                        );
                        Ty::Error
                    }
                };
                self.check(lhs, &elem);
                Ty::Bool
            }
            BinOpKind::SubsetEq => {
                let l = self.infer(lhs);
                match &l {
                    Ty::Set(_) | Ty::Error => {
                        self.check(rhs, &l);
                    }
                    other => {
                        self.err(
                            lhs.span,
                            format!("left operand of `\\subseteq` must be a set, found `{}`", other),
                            None,
                        );
                        self.infer(rhs);
                    }
                }
                Ty::Bool
            }
            BinOpKind::Cup | BinOpKind::Cap | BinOpKind::Diff => {
                let l = self.infer(lhs);
                match &l {
                    Ty::Set(_) | Ty::Error => {
                        self.check(rhs, &l);
                        l
                    }
                    other => {
                        self.err(
                            lhs.span,
                            format!(
                                "left operand of `{}` must be a set, found `{}`",
                                binop_str(op),
                                other
                            ),
                            None,
                        );
                        self.infer(rhs);
                        Ty::Error
                    }
                }
            }
            BinOpKind::Cartesian => {
                let l = self.infer(lhs);
                let r = self.infer(rhs);
                match (&l, &r) {
                    (Ty::Set(a), Ty::Set(b)) => {
                        Ty::Set(Box::new(Ty::Tuple(vec![(**a).clone(), (**b).clone()])))
                    }
                    (Ty::Error, _) | (_, Ty::Error) => Ty::Error,
                    _ => {
                        self.err(
                            span,
                            format!(
                                "operands of `\\X` must be sets, found `{}` and `{}`",
                                l, r
                            ),
                            None,
                        );
                        Ty::Error
                    }
                }
            }
        }
    }

    fn infer_unop(&mut self, op: UnOpKind, operand: &Expr, span: Span) -> Ty {
        match op {
            UnOpKind::Not => {
                self.check(operand, &Ty::Bool);
                Ty::Bool
            }
            UnOpKind::Neg => {
                let t = self.infer(operand);
                match &t {
                    Ty::Int | Ty::Float | Ty::Decimal(_) | Ty::Error => t,
                    other => {
                        self.err(
                            span,
                            format!("unary `-` requires a numeric operand, found `{}`", other),
                            None,
                        );
                        Ty::Error
                    }
                }
            }
        }
    }

    /// `e as T` — only the §2.4 whitelist.
    fn infer_cast(&mut self, expr: &Expr, ty: &Type, span: Span) -> Ty {
        let from = self.infer(expr);
        let generics = self.cur_generics.clone();
        let to = self.ty_of(ty, &generics);
        let ok = matches!(
            (&from, &to),
            (Ty::Error, _)
                | (_, Ty::Error)
                | (Ty::Int, Ty::Float)
                | (Ty::Float, Ty::Int)
                | (Ty::Int, Ty::Decimal(_))
                | (Ty::Decimal(_), Ty::Int)
                | (Ty::Decimal(_), Ty::Float)
                | (Ty::Decimal(_), Ty::Decimal(_))
        );
        if !ok {
            self.err(
                span,
                format!("cannot cast `{}` to `{}`", from, to),
                Some(
                    "allowed casts (§2.4): int ↔ float, int ↔ decimal, decimal ↔ float, and decimal precision changes"
                        .to_string(),
                ),
            );
        }
        to
    }

    fn infer_str_interp(&mut self, parts: &[StrPart], _span: Span) -> Ty {
        for p in parts {
            if let StrPart::Interp(inner) = p {
                let t = self.infer(inner);
                let basic = matches!(
                    t,
                    Ty::Bool | Ty::Int | Ty::Float | Ty::String | Ty::Date | Ty::Decimal(_) | Ty::Error
                );
                if !basic {
                    self.err(
                        inner.span,
                        format!(
                            "interpolated expression must have a basic type (bool, int, float, string, date, or decimal), found `{}`",
                            t
                        ),
                        Some("convert it with a `to_string_*` function (appendix B)".to_string()),
                    );
                }
            }
        }
        Ty::String
    }

    // ---- try (?) --------------------------------------------------------------------

    /// `e?` (§4.6): `e: option<T>` yields `T`; legal only when the innermost
    /// enclosing operator/lambda body returns `option<U>` — never in actions.
    fn infer_try(&mut self, inner: &Expr, span: Span) -> Ty {
        let t = self.infer(inner);
        let inner_ty = match t {
            Ty::Option(t) => *t,
            Ty::Error => Ty::Error,
            other => {
                self.err(
                    span,
                    format!("the `?` operator requires an `option<T>` operand, found `{}`", other),
                    None,
                );
                Ty::Error
            }
        };
        match self.body_rets.last().cloned() {
            Some((Ty::Option(_), _)) => {}
            Some((Ty::Error, _)) => self.err(
                span,
                "cannot use `?` here: the enclosing return type is not yet known",
                Some("add a return type annotation to the enclosing lambda".to_string()),
            ),
            Some((ret, ctx)) => {
                let is_action = ctx == RetCtx::Operator(EffectLevel::Action)
                    || matches!(&ret, Ty::Set(w) if matches!(**w, Ty::WriteOp));
                if is_action {
                    self.err(
                        span,
                        "`?` cannot be used in an `action` body (actions return `set<write_op>`, not `option<T>`)",
                        Some("use `match` on the option instead (§4.6)".to_string()),
                    );
                } else {
                    self.err(
                        span,
                        format!(
                            "`?` is only allowed in a body returning `option<T>`, but this body returns `{}`",
                            ret
                        ),
                        Some("use `match` to handle the `none` case (§4.6)".to_string()),
                    );
                }
            }
            None => self.err(span, "`?` is not allowed here", None),
        }
        inner_ty
    }

    // ---- write constructors ---------------------------------------------------------

    /// `insert`/`update`/`delete` checked against the table's derived types
    /// at the construction point (§3.6).
    fn infer_write_con(&mut self, w: &WriteCon) -> Ty {
        match w {
            WriteCon::Insert { table, row } => {
                if self.tables.contains_key(&table.node) {
                    let row_ty = Ty::Row(table.node.clone());
                    self.check(row, &row_ty);
                }
            }
            WriteCon::Update { table, key, transform } => {
                if let Some(info) = self.tables.get(&table.node).cloned() {
                    self.check(key, &info.key_ty);
                    let f = Ty::Fun(
                        Box::new(info.value_ty.clone()),
                        Box::new(info.value_ty),
                    );
                    self.check(transform, &f);
                }
            }
            WriteCon::Delete { table, key } => {
                if let Some(info) = self.tables.get(&table.node).cloned() {
                    self.check(key, &info.key_ty);
                }
            }
        }
        Ty::WriteOp
    }

    // ---- enum construction ----------------------------------------------------------

    /// `Variant(args)`: check payloads against the variant's scheme and
    /// instantiate the enum's type parameters (§3.2). An expected enum type
    /// may bind parameters the payload does not determine.
    fn enum_construct(&mut self, e: &Expr, name: &Ident, args: &[Expr], expected: Option<&Ty>) -> Ty {
        let Some(ename) = self.variant_enum.get(&name.node).cloned() else {
            // Imported variant or a resolution failure: unknown payload.
            for a in args {
                self.infer(a);
            }
            return Ty::Error;
        };
        let info = self.enums.get(&ename).cloned().unwrap();
        let payload = info.variants.get(&name.node).cloned().unwrap();
        let mut subst: Subst = Subst::new();
        if let Some(Ty::Enum { name: en, args: eargs }) = expected {
            if en == &ename {
                for (p, a) in info.params.iter().zip(eargs) {
                    subst.insert(p.clone(), a.clone());
                }
            }
        }
        let pstys: Vec<STy> = match &payload {
            PayloadSty::Unit => vec![],
            PayloadSty::Tuple(ts) => ts.clone(),
            PayloadSty::Record(fs) => vec![STy::Record(fs.clone())],
        };
        for (sty, arg) in pstys.iter().zip(args) {
            self.check_arg_scheme(sty, arg, &mut subst);
        }
        let mut tys = Vec::new();
        for p in &info.params {
            match subst.get(p) {
                Some(t) => tys.push(t.clone()),
                None => {
                    self.err(
                        e.span,
                        format!(
                            "cannot infer type argument `{}` of enum `{}`",
                            p, ename
                        ),
                        Some("add a type annotation at the use site".to_string()),
                    );
                    tys.push(Ty::Error);
                }
            }
        }
        Ty::Enum { name: ename, args: tys }
    }

    // ---- match ----------------------------------------------------------------------

    /// Branches: infer the first arm's body, check the rest against it (or
    /// check every arm against the expected type); then exhaustiveness.
    fn infer_match(
        &mut self,
        e: &Expr,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: Option<&Ty>,
    ) -> Ty {
        let st = self.infer(scrutinee);
        let mut first_ty: Option<Ty> = None;
        for arm in arms {
            self.env.push(HashMap::new());
            self.bind_pat(&arm.pat, &st);
            match (expected, &first_ty) {
                (Some(exp), _) => {
                    self.check(&arm.body, exp);
                }
                (None, None) => first_ty = Some(self.infer(&arm.body)),
                (None, Some(t)) => {
                    let t = t.clone();
                    self.check(&arm.body, &t);
                }
            }
            self.env.pop();
        }
        self.check_exhaustive(&st, arms, e.span);
        expected.cloned().or(first_ty).unwrap_or(Ty::Error)
    }

    /// Is this pattern total (matches every value) for the given type?
    fn pat_total(&self, pat: &Pattern, ty: &Ty) -> bool {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Bind(_) => true,
            PatternKind::Tuple(pats) => match ty {
                Ty::Tuple(ts) if ts.len() == pats.len() => {
                    pats.iter().zip(ts).all(|(p, t)| self.pat_total(p, t))
                }
                _ => false,
            },
            // A record pattern binds existing fields; it always matches a
            // record/row of that type.
            PatternKind::Record(_) => matches!(ty, Ty::Record(_) | Ty::Row(_)),
            _ => false,
        }
    }

    fn check_exhaustive(&mut self, scrut: &Ty, arms: &[MatchArm], span: Span) {
        if arms.iter().any(|a| self.pat_total(&a.pat, scrut)) {
            return;
        }
        let help = Some("add the missing arm(s) or a wildcard `_` / binding pattern".to_string());
        match scrut {
            Ty::Error => {}
            Ty::Option(_) => {
                let has_none = arms.iter().any(|a| matches!(a.pat.kind, PatternKind::None));
                let has_some = arms.iter().any(|a| matches!(a.pat.kind, PatternKind::Some(_)));
                if !has_none || !has_some {
                    let mut missing = Vec::new();
                    if !has_some {
                        missing.push("`some(_)`");
                    }
                    if !has_none {
                        missing.push("`none`");
                    }
                    self.err(
                        span,
                        format!("non-exhaustive match on `{}`: missing {}", scrut, missing.join(" and ")),
                        help,
                    );
                }
            }
            Ty::Enum { name, .. } => {
                if let Some(info) = self.enums.get(name).cloned() {
                    let missing: Vec<String> = info
                        .order
                        .iter()
                        .filter(|v| {
                            !arms.iter().any(|a| {
                                matches!(&a.pat.kind, PatternKind::Variant { name: vn, .. } if vn.node == **v)
                            })
                        })
                        .map(|v| format!("`{}`", v))
                        .collect();
                    if !missing.is_empty() {
                        self.err(
                            span,
                            format!(
                                "non-exhaustive match on `{}`: missing variant(s) {}",
                                scrut,
                                missing.join(", ")
                            ),
                            help,
                        );
                    }
                }
            }
            Ty::Bool => {
                let has_true = arms
                    .iter()
                    .any(|a| matches!(a.pat.kind, PatternKind::Lit(PatLit::Bool(true))));
                let has_false = arms
                    .iter()
                    .any(|a| matches!(a.pat.kind, PatternKind::Lit(PatLit::Bool(false))));
                if !(has_true && has_false) {
                    self.err(span, "non-exhaustive match on `bool`", help);
                }
            }
            Ty::Vector(_) => {
                let has_nil = arms.iter().any(|a| matches!(a.pat.kind, PatternKind::ConsNil));
                let has_cons = arms.iter().any(|a| matches!(a.pat.kind, PatternKind::Cons { .. }));
                if !(has_nil && has_cons) {
                    self.err(
                        span,
                        format!("non-exhaustive match on `{}`: needs both `[]` and `[h, ..t]` arms", scrut),
                        help,
                    );
                }
            }
            _ => self.err(
                span,
                format!(
                    "match on `{}` is not exhaustive; literal patterns cannot cover this type",
                    scrut
                ),
                help,
            ),
        }
    }

    // ---- patterns -------------------------------------------------------------------

    /// Bind a pattern against a type, checking shape compatibility.
    fn bind_pat(&mut self, pat: &Pattern, ty: &Ty) {
        if matches!(ty, Ty::Error) {
            for id in pat.bound_idents() {
                self.bind(id, Ty::Error);
            }
            return;
        }
        match &pat.kind {
            PatternKind::Wildcard => {}
            PatternKind::Bind(name) => self.bind(name, ty.clone()),
            PatternKind::Lit(l) => {
                let lt = match l {
                    PatLit::Int(_) => Ty::Int,
                    PatLit::Str(_) => Ty::String,
                    PatLit::Bool(_) => Ty::Bool,
                };
                self.mismatch(pat.span, &lt, ty);
            }
            PatternKind::None => {
                if !matches!(ty, Ty::Option(_)) {
                    self.err(
                        pat.span,
                        format!("pattern `none` does not match type `{}`", ty),
                        None,
                    );
                }
            }
            PatternKind::Some(inner) => match ty {
                Ty::Option(t) => {
                    let t = t.clone();
                    self.bind_pat(inner, &t);
                }
                _ => self.err(
                    pat.span,
                    format!("pattern `some(_)` does not match type `{}`", ty),
                    None,
                ),
            },
            PatternKind::Variant { name, args } => self.bind_variant_pat(pat, name, args, ty),
            PatternKind::Tuple(pats) => match ty {
                Ty::Tuple(ts) if ts.len() == pats.len() => {
                    for (p, t) in pats.iter().zip(ts.clone()) {
                        self.bind_pat(p, &t);
                    }
                }
                _ => self.err(
                    pat.span,
                    format!("tuple pattern does not match type `{}`", ty),
                    None,
                ),
            },
            PatternKind::Record(names) => match self.fields_of(ty) {
                Some(fs) => {
                    for n in names {
                        match fs.iter().find(|(f, _)| f == &n.node) {
                            Some((_, t)) => self.bind(n, t.clone()),
                            None => {
                                self.err(
                                    n.span,
                                    format!("type `{}` has no field `{}`", ty, n.node),
                                    None,
                                );
                                self.bind(n, Ty::Error);
                            }
                        }
                    }
                }
                None => self.err(
                    pat.span,
                    format!("record pattern does not match type `{}`", ty),
                    None,
                ),
            },
            PatternKind::ConsNil => {
                if !matches!(ty, Ty::Vector(_)) {
                    self.err(
                        pat.span,
                        format!("pattern `[]` does not match type `{}`", ty),
                        None,
                    );
                }
            }
            PatternKind::Cons { head, tail } => match ty {
                Ty::Vector(t) => {
                    let elem: Ty = (**t).clone();
                    self.bind_pat(head, &elem);
                    self.bind_pat(tail, &Ty::Vector(Box::new(elem)));
                }
                _ => self.err(
                    pat.span,
                    format!("cons pattern does not match type `{}`", ty),
                    None,
                ),
            },
        }
    }

    fn bind_variant_pat(&mut self, pat: &Pattern, name: &Ident, args: &[Pattern], ty: &Ty) {
        let Some(ename) = self.variant_enum.get(&name.node).cloned() else {
            return; // imported variant or resolve failure
        };
        let info = self.enums.get(&ename).cloned().unwrap();
        let eargs = match ty {
            Ty::Enum { name: n, args } if n == &ename => args.clone(),
            _ => {
                self.err(
                    pat.span,
                    format!("pattern `{}` does not match type `{}`", name.node, ty),
                    None,
                );
                return;
            }
        };
        let subst: Subst = info.params.iter().cloned().zip(eargs).collect();
        let payload = info.variants.get(&name.node).cloned().unwrap();
        let pstys: Vec<STy> = match &payload {
            PayloadSty::Unit => vec![],
            PayloadSty::Tuple(ts) => ts.clone(),
            PayloadSty::Record(fs) => vec![STy::Record(fs.clone())],
        };
        if pstys.len() != args.len() {
            self.err(
                name.span,
                format!(
                    "variant `{}` takes {} pattern argument(s), got {}",
                    name.node,
                    pstys.len(),
                    args.len()
                ),
                None,
            );
            return;
        }
        for (sty, p) in pstys.iter().zip(args) {
            if let Some(t) = self.subst_sty(sty, &subst) {
                self.bind_pat(p, &t);
            }
        }
    }
}

fn tuple_len(ty: &Ty) -> usize {
    match ty {
        Ty::Tuple(ts) => ts.len(),
        _ => 1,
    }
}

fn binop_str(op: BinOpKind) -> &'static str {
    match op {
        BinOpKind::Add => "+",
        BinOpKind::Sub => "-",
        BinOpKind::Mul => "*",
        BinOpKind::Div => "/",
        BinOpKind::Mod => "%",
        BinOpKind::Eq => "=",
        BinOpKind::Ne => "/=",
        BinOpKind::Lt => "<",
        BinOpKind::Gt => ">",
        BinOpKind::Le => "<=",
        BinOpKind::Ge => ">=",
        BinOpKind::And => "/\\",
        BinOpKind::Or => "\\/",
        BinOpKind::Impl => "=>",
        BinOpKind::In => "\\in",
        BinOpKind::SubsetEq => "\\subseteq",
        BinOpKind::Cup => "\\cup",
        BinOpKind::Cap => "\\cap",
        BinOpKind::Diff => "\\",
        BinOpKind::Cartesian => "\\X",
    }
}

/// Collect type variables occurring in `set` element / `map` key positions
/// (which require hashable, §2.3) so instantiations can be validated.
fn collect_set_key_vars(sty: &STy, out: &mut Vec<String>) {
    match sty {
        STy::Set(inner) => {
            collect_vars(inner, out);
            collect_set_key_vars(inner, out);
        }
        // `SetOrBag` may instantiate to a `bag`, whose elements need only Eq —
        // and any actual `set<T>` argument was already validated for hashable
        // elements at its construction site, so no constraint is imposed here.
        STy::SetOrBag(inner) => collect_set_key_vars(inner, out),
        STy::Map(k, v) => {
            collect_vars(k, out);
            collect_set_key_vars(k, out);
            collect_set_key_vars(v, out);
        }
        STy::Option(t) | STy::Vector(t) | STy::Bag(t) => collect_set_key_vars(t, out),
        STy::Tuple(ts) => {
            for t in ts {
                collect_set_key_vars(t, out);
            }
        }
        STy::Record(fs) => {
            for (_, t) in fs {
                collect_set_key_vars(t, out);
            }
        }
        STy::Fun(a, b) => {
            collect_set_key_vars(a, out);
            collect_set_key_vars(b, out);
        }
        STy::Enum { args, .. } => {
            for t in args {
                collect_set_key_vars(t, out);
            }
        }
        _ => {}
    }
}

fn collect_vars(sty: &STy, out: &mut Vec<String>) {
    match sty {
        STy::Var(n) => out.push(n.clone()),
        STy::Option(t) | STy::Vector(t) | STy::Set(t) | STy::Bag(t) | STy::SetOrBag(t) => {
            collect_vars(t, out)
        }
        STy::Map(k, v) => {
            collect_vars(k, out);
            collect_vars(v, out);
        }
        STy::Tuple(ts) => {
            for t in ts {
                collect_vars(t, out);
            }
        }
        STy::Record(fs) => {
            for (_, t) in fs {
                collect_vars(t, out);
            }
        }
        STy::Fun(a, b) => {
            collect_vars(a, out);
            collect_vars(b, out);
        }
        STy::Enum { args, .. } => {
            for t in args {
                collect_vars(t, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::{decl, expr, pat, ty};
    use crate::resolve::resolve_module;

    fn check(items: Vec<Item>) -> (TypedModule, DiagBag) {
        let r = resolve_module(decl::module("test", items), &[]).expect("resolve failed");
        check_module(&r)
    }

    fn msgs(bag: &DiagBag) -> Vec<String> {
        bag.errors().iter().map(|e| e.message().to_string()).collect()
    }

    fn expect_ok(items: Vec<Item>) -> TypedModule {
        let (tm, bag) = check(items);
        assert!(!bag.has_errors(), "unexpected errors:\n{}", bag.render());
        tm
    }

    fn expect_err(items: Vec<Item>) -> Vec<String> {
        let (_, bag) = check(items);
        assert!(bag.has_errors(), "expected type errors, got none");
        msgs(&bag)
    }

    fn users_table() -> Item {
        decl::table(
            "users",
            vec![("id", ty::int()), ("name", ty::string()), ("city", ty::string())],
            &["id"],
        )
    }

    fn sessions_table() -> Item {
        decl::table(
            "sessions",
            vec![("user_id", ty::int()), ("ts", ty::int()), ("duration", ty::int())],
            &["user_id", "ts"],
        )
    }

    fn shape_enum() -> Item {
        decl::enum_(
            "shape",
            vec![
                decl::variant_tuple("circle", vec![ty::float()]),
                decl::variant_tuple("rect", vec![ty::float(), ty::float()]),
            ],
        )
    }

    // ---- §2.4 casts ------------------------------------------------------------

    #[test]
    fn cast_int_to_float_ok() {
        expect_ok(vec![decl::function("f", vec![], ty::float(), expr::cast(expr::int(1), ty::float()))]);
    }

    #[test]
    fn cast_float_to_int_ok() {
        expect_ok(vec![decl::function("f", vec![], ty::int(), expr::cast(expr::float(1.5), ty::int()))]);
    }

    #[test]
    fn cast_int_decimal_both_ways_ok() {
        let to_dec = decl::function(
            "f",
            vec![],
            ty::decimal(Some((10, 2))),
            expr::cast(expr::int(1), ty::decimal(Some((10, 2)))),
        );
        let to_int = decl::function(
            "g",
            vec![],
            ty::int(),
            expr::cast(expr::decimal("1.5", None), ty::int()),
        );
        expect_ok(vec![to_dec, to_int]);
    }

    #[test]
    fn cast_decimal_to_float_ok() {
        expect_ok(vec![decl::function(
            "f",
            vec![],
            ty::float(),
            expr::cast(expr::decimal("1.5", Some((4, 2))), ty::float()),
        )]);
    }

    #[test]
    fn cast_decimal_precision_changes_ok() {
        let bounded = decl::function(
            "f",
            vec![],
            ty::decimal(Some((12, 4))),
            expr::cast(expr::decimal("1.5", Some((4, 2))), ty::decimal(Some((12, 4)))),
        );
        let to_unbounded = decl::function(
            "g",
            vec![],
            ty::decimal(None),
            expr::cast(expr::decimal("1.5", Some((4, 2))), ty::decimal(None)),
        );
        let from_unbounded = decl::function(
            "h",
            vec![],
            ty::decimal(Some((10, 2))),
            expr::cast(expr::decimal("1.5", None), ty::decimal(Some((10, 2)))),
        );
        expect_ok(vec![bounded, to_unbounded, from_unbounded]);
    }

    #[test]
    fn cast_int_to_string_errors() {
        let m = expect_err(vec![decl::function(
            "f",
            vec![],
            ty::string(),
            expr::cast(expr::int(1), ty::string()),
        )]);
        assert!(m.iter().any(|s| s.contains("cannot cast `int` to `string`")), "{m:?}");
    }

    #[test]
    fn cast_identity_not_in_whitelist_errors() {
        let m = expect_err(vec![decl::function(
            "f",
            vec![],
            ty::int(),
            expr::cast(expr::int(1), ty::int()),
        )]);
        assert!(m.iter().any(|s| s.contains("cannot cast")), "{m:?}");
    }

    // ---- §2.3 hashable / ord / eq constraints --------------------------------------

    #[test]
    fn set_of_float_errors() {
        let body = expr::block(
            vec![expr::let_(pat::bind("s"), expr::set_lit(vec![expr::float(1.0)]))],
            expr::bool_(true),
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::bool_(), body)]);
        assert!(m.iter().any(|s| s.contains("not hashable")), "{m:?}");
    }

    #[test]
    fn map_with_float_key_errors() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("m"),
                expr::map_lit(vec![(expr::float(1.0), expr::int(1))]),
            )],
            expr::bool_(true),
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::bool_(), body)]);
        assert!(m.iter().any(|s| s.contains("map keys") && s.contains("not hashable")), "{m:?}");
    }

    #[test]
    fn bag_of_float_ok() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("b"),
                expr::bag_lit(vec![expr::float(1.0), expr::float(2.0)]),
            )],
            expr::bool_(true),
        );
        expect_ok(vec![decl::function("f", vec![], ty::bool_(), body)]);
    }

    #[test]
    fn sort_by_record_key_errors() {
        let rec = ty::record(vec![("a", ty::int())]);
        let body = expr::call(
            "sort_by",
            vec![expr::var("xs"), expr::lambda(&[], vec![pat::bind("r")], expr::var("r"))],
        );
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("xs", ty::vector(rec.clone()))],
            ty::vector(rec),
            body,
        )]);
        assert!(
            m.iter().any(|s| s.contains("`K` of `sort_by`") && s.contains("not ordered")),
            "{m:?}"
        );
    }

    #[test]
    fn aggregate_float_group_key_errors() {
        let body = expr::call_args(
            "count_by",
            vec![
                expr::named_arg("src", expr::var("src")),
                expr::named_arg("key", expr::lambda(&[], vec![pat::bind("x")], expr::float(1.0))),
            ],
        );
        let ret = ty::vector(ty::record(vec![("key", ty::float()), ("agg", ty::int())]));
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("src", ty::set(ty::int()))],
            ret,
            body,
        )]);
        assert!(
            m.iter().any(|s| s.contains("`K` of `count_by`") && s.contains("not hashable")),
            "{m:?}"
        );
    }

    #[test]
    fn eq_on_function_type_errors() {
        let fty = ty::fun(ty::int(), ty::int());
        let body = expr::binop(BinOpKind::Eq, expr::var("g"), expr::var("g"));
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("g", fty)],
            ty::bool_(),
            body,
        )]);
        assert!(m.iter().any(|s| s.contains("does not support equality")), "{m:?}");
    }

    #[test]
    fn record_comparison_not_ord_errors() {
        let rec = ty::record(vec![("x", ty::int())]);
        let body = expr::binop(BinOpKind::Lt, expr::var("a"), expr::var("b"));
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("a", rec.clone()), decl::param("b", rec)],
            ty::bool_(),
            body,
        )]);
        assert!(m.iter().any(|s| s.contains("not ordered")), "{m:?}");
    }

    #[test]
    fn tuple_comparison_ok() {
        let tup = ty::tuple(vec![ty::int(), ty::int()]);
        let body = expr::binop(BinOpKind::Lt, expr::var("a"), expr::var("b"));
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("a", tup.clone()), decl::param("b", tup)],
            ty::bool_(),
            body,
        )]);
    }

    // ---- §2.2 table derived types & §4.3/§3.6 primitives ---------------------------

    #[test]
    fn table_derived_key_value_row_ok() {
        let lookup_fn = decl::query(
            "q",
            vec![],
            ty::option(ty::value("users")),
            expr::call("lookup", vec![expr::var("users"), expr::int(1)]),
        );
        let key_fn = decl::function("k", vec![], ty::key("users"), expr::int(1));
        let row_field = decl::function(
            "g",
            vec![decl::param("u", ty::named("users"))],
            ty::int(),
            expr::field(expr::var("u"), "id"),
        );
        let tm = expect_ok(vec![users_table(), lookup_fn, key_fn, row_field]);
        // `value users` = record of the non-key fields.
        assert_eq!(
            tm.operator_sigs["q"].ret,
            Ty::Option(Box::new(Ty::Record(vec![
                ("name".into(), Ty::String),
                ("city".into(), Ty::String),
            ])))
        );
        assert_eq!(tm.operator_sigs["k"].ret, Ty::Int);
    }

    #[test]
    fn composite_key_is_tuple_ok() {
        let q = decl::query(
            "q",
            vec![],
            ty::option(ty::value("sessions")),
            expr::call(
                "lookup",
                vec![expr::var("sessions"), expr::tuple(vec![expr::int(1), expr::int(2)])],
            ),
        );
        let tm = expect_ok(vec![sessions_table(), q]);
        assert_eq!(
            tm.operator_sigs["q"].ret,
            Ty::Option(Box::new(Ty::Record(vec![("duration".into(), Ty::Int)])))
        );
    }

    #[test]
    fn lookup_wrong_key_type_errors() {
        let q = decl::query(
            "q",
            vec![],
            ty::option(ty::value("users")),
            expr::call("lookup", vec![expr::var("users"), expr::str_("a")]),
        );
        let m = expect_err(vec![users_table(), q]);
        assert!(
            m.iter().any(|s| s.contains("expected `int`, found `string`")),
            "{m:?}"
        );
    }

    #[test]
    fn read_signature_ok() {
        let q = decl::query(
            "q",
            vec![],
            ty::set(ty::named("users")),
            expr::call(
                "read",
                vec![
                    expr::var("users"),
                    expr::lambda(
                        &[],
                        vec![pat::bind("u")],
                        expr::binop(BinOpKind::Eq, expr::field(expr::var("u"), "id"), expr::int(1)),
                    ),
                ],
            ),
        );
        expect_ok(vec![users_table(), q]);
    }

    #[test]
    fn read_predicate_must_be_bool_errors() {
        let q = decl::query(
            "q",
            vec![],
            ty::set(ty::named("users")),
            expr::call(
                "read",
                vec![
                    expr::var("users"),
                    expr::lambda(&[], vec![pat::bind("u")], expr::field(expr::var("u"), "id")),
                ],
            ),
        );
        let m = expect_err(vec![users_table(), q]);
        assert!(m.iter().any(|s| s.contains("expected `bool`, found `int`")), "{m:?}");
    }

    #[test]
    fn table_sugar_generator_binds_row_ok() {
        let body = expr::set_map(
            expr::field(expr::var("u"), "id"),
            vec![expr::gen(pat::bind("u"), expr::var("users"))],
        );
        expect_ok(vec![users_table(), decl::query("q", vec![], ty::set(ty::int()), body)]);
    }

    #[test]
    fn rows_are_hashable_despite_float_fields_ok() {
        let orders = decl::table(
            "orders",
            vec![("order_id", ty::int()), ("amount", ty::float())],
            &["order_id"],
        );
        let q = decl::query(
            "q",
            vec![],
            ty::set(ty::named("orders")),
            expr::call(
                "read",
                vec![expr::var("orders"), expr::lambda(&[], vec![pat::bind("o")], expr::bool_(true))],
            ),
        );
        expect_ok(vec![orders, q]);
    }

    #[test]
    fn insert_row_checked_ok_and_missing_field_errors() {
        let good = expr::set_lit(vec![expr::call(
            "insert",
            vec![
                expr::var("users"),
                expr::record_lit(vec![
                    ("id", expr::int(1)),
                    ("name", expr::str_("a")),
                    ("city", expr::str_("c")),
                ]),
            ],
        )]);
        expect_ok(vec![users_table(), decl::action("a", vec![], good)]);

        let bad = expr::set_lit(vec![expr::call(
            "insert",
            vec![
                expr::var("users"),
                expr::record_lit(vec![("id", expr::int(1)), ("name", expr::str_("a"))]),
            ],
        )]);
        let m = expect_err(vec![users_table(), decl::action("a", vec![], bad)]);
        assert!(m.iter().any(|s| s.contains("missing field(s) `city`")), "{m:?}");
    }

    #[test]
    fn update_delete_signatures_ok_and_key_errors() {
        let upd = expr::set_lit(vec![expr::call(
            "update",
            vec![
                expr::var("users"),
                expr::int(1),
                expr::lambda(
                    &[],
                    vec![pat::bind("v")],
                    expr::record_upd(expr::var("v"), vec![("name", expr::str_("b"))]),
                ),
            ],
        )]);
        let del = expr::set_lit(vec![expr::call("delete", vec![expr::var("users"), expr::int(1)])]);
        expect_ok(vec![users_table(), decl::action("a", vec![], upd.clone())]);
        expect_ok(vec![users_table(), decl::action("b", vec![], del)]);

        let bad_key = expr::set_lit(vec![expr::call(
            "update",
            vec![
                expr::var("users"),
                expr::str_("x"),
                expr::lambda(&[], vec![pat::bind("v")], expr::var("v")),
            ],
        )]);
        let m = expect_err(vec![users_table(), decl::action("c", vec![], bad_key)]);
        assert!(
            m.iter().any(|s| s.contains("expected `int`, found `string`")),
            "{m:?}"
        );
    }

    #[test]
    fn update_transform_must_be_value_to_value_errors() {
        let bad = expr::set_lit(vec![expr::call(
            "update",
            vec![
                expr::var("users"),
                expr::int(1),
                expr::lambda(&[], vec![pat::bind("v")], expr::int(1)),
            ],
        )]);
        let m = expect_err(vec![users_table(), decl::action("a", vec![], bad)]);
        assert!(m.iter().any(|s| s.contains("type mismatch")), "{m:?}");
    }

    #[test]
    fn action_body_must_be_set_write_op_errors() {
        let body = expr::set_lit(vec![expr::int(1)]);
        let m = expect_err(vec![decl::action("a", vec![], body)]);
        assert!(m.iter().any(|s| s.contains("expected `write_op`, found `int`")), "{m:?}");
    }

    #[test]
    fn table_primary_key_must_be_key_type_errors() {
        let bad = decl::table("bad", vec![("id", ty::float())], &["id"]);
        let m = expect_err(vec![bad]);
        assert!(m.iter().any(|s| s.contains("not a valid key type")), "{m:?}");
    }

    #[test]
    fn foreign_key_column_type_checked() {
        let orders_ok = Item::Table(TableDecl {
            vis: Visibility::Private,
            name: crate::ast::builder::id("orders"),
            fields: vec![
                (crate::ast::builder::id("order_id"), ty::int()),
                (crate::ast::builder::id("user_id"), ty::int()),
            ],
            pk: vec![crate::ast::builder::id("order_id")],
            fks: vec![FkClause {
                cols: vec![crate::ast::builder::id("user_id")],
                references: crate::ast::builder::id("users"),
            }],
        });
        expect_ok(vec![users_table(), orders_ok]);

        let orders_bad = Item::Table(TableDecl {
            vis: Visibility::Private,
            name: crate::ast::builder::id("orders"),
            fields: vec![
                (crate::ast::builder::id("order_id"), ty::int()),
                (crate::ast::builder::id("user_id"), ty::string()),
            ],
            pk: vec![crate::ast::builder::id("order_id")],
            fks: vec![FkClause {
                cols: vec![crate::ast::builder::id("user_id")],
                references: crate::ast::builder::id("users"),
            }],
        });
        let m = expect_err(vec![users_table(), orders_bad]);
        assert!(m.iter().any(|s| s.contains("foreign key column `user_id`")), "{m:?}");
    }

    // ---- generic instantiation (§2.5, §4.8, appendix B) -----------------------------

    fn fold_sum() -> (Expr, Span) {
        let c = expr::call(
            "fold",
            vec![
                expr::vector(vec![expr::int(1), expr::int(2)]),
                expr::int(0),
                expr::lambda(
                    &[],
                    vec![pat::bind("acc"), pat::bind("x")],
                    expr::binop(BinOpKind::Add, expr::var("acc"), expr::var("x")),
                ),
            ],
        );
        let ExprKind::Call(call) = &c.kind else { unreachable!() };
        let span = call.name.span;
        (c, span)
    }

    #[test]
    fn fold_instantiation_and_lambda_param_inference_ok() {
        let (c, span) = fold_sum();
        let tm = expect_ok(vec![decl::function("f", vec![], ty::int(), c)]);
        assert_eq!(
            tm.instantiations.get(&span),
            Some(&vec![("A".to_string(), Ty::Int), ("T".to_string(), Ty::Int)])
        );
        // Lambda parameters were inferred from the expected signature.
        let locals = &tm.operator_locals["f"];
        assert!(locals.contains(&("acc".to_string(), Ty::Int)), "{locals:?}");
        assert!(locals.contains(&("x".to_string(), Ty::Int)), "{locals:?}");
    }

    #[test]
    fn map_infers_result_type_param_ok() {
        let body = expr::call(
            "map",
            vec![
                expr::vector(vec![expr::int(1)]),
                expr::lambda(
                    &[],
                    vec![pat::bind("x")],
                    expr::binop(BinOpKind::Add, expr::var("x"), expr::int(1)),
                ),
            ],
        );
        expect_ok(vec![decl::function("f", vec![], ty::vector(ty::int()), body)]);
    }

    #[test]
    fn aggregate_named_args_ok() {
        let body = expr::call_args(
            "aggregate",
            vec![
                expr::named_arg("source", expr::var("src")),
                expr::named_arg("group_key", expr::lambda(&[], vec![pat::bind("x")], expr::var("x"))),
                expr::named_arg(
                    "value",
                    expr::lambda(&[], vec![pat::bind("x")], expr::cast(expr::var("x"), ty::float())),
                ),
                expr::named_arg(
                    "reducer",
                    expr::lambda(
                        &[],
                        vec![pat::tuple(vec![pat::bind("a"), pat::bind("b")])],
                        expr::binop(BinOpKind::Add, expr::var("a"), expr::var("b")),
                    ),
                ),
                expr::named_arg("init", expr::float(0.0)),
                expr::named_arg("finalize", expr::lambda(&[], vec![pat::bind("v")], expr::var("v"))),
            ],
        );
        let ret = ty::vector(ty::record(vec![("key", ty::int()), ("agg", ty::float())]));
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("src", ty::set(ty::int()))],
            ret,
            body,
        )]);
    }

    #[test]
    fn count_by_sugar_ok() {
        let body = expr::call_args(
            "count_by",
            vec![
                expr::named_arg("src", expr::var("src")),
                expr::named_arg("key", expr::lambda(&[], vec![pat::bind("x")], expr::var("x"))),
            ],
        );
        let ret = ty::vector(ty::record(vec![("key", ty::int()), ("agg", ty::int())]));
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("src", ty::set(ty::int()))],
            ret,
            body,
        )]);
    }

    #[test]
    fn turbofish_ok_and_inconsistent_errors() {
        let (ok_call, _) = fold_sum();
        let ExprKind::Call(c) = &ok_call.kind else { unreachable!() };
        let ok = Expr::new(
            ExprKind::Call(Call {
                name: c.name.clone(),
                type_args: Some(vec![ty::int(), ty::int()]),
                args: c.args.clone(),
            }),
            ok_call.span,
        );
        expect_ok(vec![decl::function("f", vec![], ty::int(), ok)]);

        let (bad_call, _) = fold_sum();
        let ExprKind::Call(c) = &bad_call.kind else { unreachable!() };
        let bad = Expr::new(
            ExprKind::Call(Call {
                name: c.name.clone(),
                type_args: Some(vec![ty::int(), ty::string()]),
                args: c.args.clone(),
            }),
            bad_call.span,
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), bad)]);
        assert!(m.iter().any(|s| s.contains("argument type mismatch")), "{m:?}");
    }

    #[test]
    fn turbofish_on_non_generic_errors() {
        let body = expr::call_ty("max", vec![ty::int()], vec![expr::int(1), expr::int(2)]);
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), body)]);
        assert!(m.iter().any(|s| s.contains("is not generic")), "{m:?}");
    }

    #[test]
    fn user_generic_function_instantiated_ok() {
        let id_fn = decl::function_gen(
            "ident",
            &["T"],
            vec![decl::param("x", ty::named("T"))],
            ty::named("T"),
            expr::var("x"),
        );
        let call = expr::call("ident", vec![expr::int(1)]);
        let ExprKind::Call(c) = &call.kind else { unreachable!() };
        let span = c.name.span;
        let f = decl::function("f", vec![], ty::int(), call);
        let g = decl::function(
            "g",
            vec![],
            ty::int(),
            expr::call_ty("ident", vec![ty::int()], vec![expr::int(2)]),
        );
        let tm = expect_ok(vec![id_fn, f, g]);
        assert_eq!(
            tm.instantiations.get(&span),
            Some(&vec![("T".to_string(), Ty::Int)])
        );
    }

    #[test]
    fn undetermined_type_param_errors() {
        let make = decl::function_ext("make", &["T"], vec![], ty::named("T"));
        // No annotation on the let binding: nothing determines `T`.
        let body = expr::block(
            vec![expr::let_(pat::bind("x"), expr::call("make", vec![]))],
            expr::int(0),
        );
        let f = decl::function("f", vec![], ty::int(), body);
        let m = expect_err(vec![make, f]);
        assert!(
            m.iter().any(|s| s.contains("cannot infer type parameter(s) `T` for `make`")),
            "{m:?}"
        );
    }

    // ---- §4.6 match / ? ----------------------------------------------------------------

    #[test]
    fn match_enum_exhaustive_ok() {
        let body = expr::match_(
            expr::var("s"),
            vec![
                (pat::variant("circle", vec![pat::bind("r")]), expr::str_("c")),
                (
                    pat::variant("rect", vec![pat::bind("a"), pat::bind("b")]),
                    expr::str_("r"),
                ),
            ],
        );
        expect_ok(vec![
            shape_enum(),
            decl::function("f", vec![decl::param("s", ty::named("shape"))], ty::string(), body),
        ]);
    }

    #[test]
    fn match_enum_missing_variant_errors() {
        let body = expr::match_(
            expr::var("s"),
            vec![(pat::variant("circle", vec![pat::bind("r")]), expr::str_("c"))],
        );
        let m = expect_err(vec![
            shape_enum(),
            decl::function("f", vec![decl::param("s", ty::named("shape"))], ty::string(), body),
        ]);
        assert!(
            m.iter().any(|s| s.contains("non-exhaustive") && s.contains("`rect`")),
            "{m:?}"
        );
    }

    #[test]
    fn match_wildcard_fallback_ok() {
        let body = expr::match_(
            expr::var("s"),
            vec![
                (pat::variant("circle", vec![pat::bind("r")]), expr::str_("c")),
                (pat::wild(), expr::str_("other")),
            ],
        );
        expect_ok(vec![
            shape_enum(),
            decl::function("f", vec![decl::param("s", ty::named("shape"))], ty::string(), body),
        ]);
    }

    #[test]
    fn match_option_ok_and_missing_none_errors() {
        let ok_body = expr::match_(
            expr::var("o"),
            vec![
                (pat::some(pat::bind("x")), expr::var("x")),
                (pat::none(), expr::int(0)),
            ],
        );
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("o", ty::option(ty::int()))],
            ty::int(),
            ok_body,
        )]);

        let bad_body = expr::match_(
            expr::var("o"),
            vec![(pat::some(pat::bind("x")), expr::var("x"))],
        );
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("o", ty::option(ty::int()))],
            ty::int(),
            bad_body,
        )]);
        assert!(m.iter().any(|s| s.contains("non-exhaustive") && s.contains("`none`")), "{m:?}");
    }

    #[test]
    fn match_arm_types_must_agree_errors() {
        let body = expr::match_(
            expr::var("x"),
            vec![
                (pat::lit_int(1), expr::int(1)),
                (pat::wild(), expr::str_("a")),
            ],
        );
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("x", ty::int())],
            ty::int(),
            body,
        )]);
        assert!(m.iter().any(|s| s.contains("expected `int`, found `string`")), "{m:?}");
    }

    #[test]
    fn try_in_option_query_ok() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("u"),
                expr::try_(expr::call("lookup", vec![expr::var("users"), expr::var("id")])),
            )],
            expr::some(expr::field(expr::var("u"), "name")),
        );
        expect_ok(vec![
            users_table(),
            decl::query(
                "q",
                vec![decl::param("id", ty::int())],
                ty::option(ty::string()),
                body,
            ),
        ]);
    }

    #[test]
    fn try_in_action_errors() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("u"),
                expr::try_(expr::call("lookup", vec![expr::var("users"), expr::var("id")])),
            )],
            expr::set_lit(vec![expr::call(
                "delete",
                vec![expr::var("users"), expr::var("id")],
            )]),
        );
        let m = expect_err(vec![
            users_table(),
            decl::action("a", vec![decl::param("id", ty::int())], body),
        ]);
        assert!(m.iter().any(|s| s.contains("cannot be used in an `action`")), "{m:?}");
    }

    #[test]
    fn try_on_non_option_errors() {
        let body = expr::block(
            vec![expr::let_(pat::bind("x"), expr::try_(expr::int(1)))],
            expr::some(expr::int(2)),
        );
        let m = expect_err(vec![decl::query("q", vec![], ty::option(ty::int()), body)]);
        assert!(m.iter().any(|s| s.contains("requires an `option<T>` operand")), "{m:?}");
    }

    // ---- lambdas ---------------------------------------------------------------------

    #[test]
    fn lambda_unannotated_without_expected_errors() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("g"),
                expr::lambda(&[], vec![pat::bind("x")], expr::var("x")),
            )],
            expr::int(0),
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), body)]);
        assert!(
            m.iter().any(|s| s.contains("cannot infer the type of this lambda parameter")),
            "{m:?}"
        );
    }

    #[test]
    fn lambda_annotation_first_class_call_ok() {
        let body = expr::block(
            vec![expr::let_(
                pat::bind("inc"),
                expr::lambda_ann(
                    &[],
                    vec![(pat::bind("x"), Some(ty::int()))],
                    None,
                    expr::binop(BinOpKind::Add, expr::var("x"), expr::int(1)),
                ),
            )],
            expr::call("inc", vec![expr::int(1)]),
        );
        expect_ok(vec![decl::function("f", vec![], ty::int(), body)]);
    }

    // ---- method-call sugar (§4.1, A.3) ------------------------------------------------

    #[test]
    fn method_length_string_and_vector_ok() {
        let s = decl::function("f", vec![], ty::int(), expr::method_call(expr::str_("a"), "length", vec![]));
        let v = decl::function(
            "g",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            expr::method_call(expr::var("xs"), "length", vec![]),
        );
        expect_ok(vec![s, v]);
    }

    #[test]
    fn method_map_option_ok() {
        let body = expr::method_call(
            expr::var("o"),
            "map",
            vec![expr::lambda(
                &[],
                vec![pat::bind("x")],
                expr::binop(BinOpKind::Add, expr::var("x"), expr::int(1)),
            )],
        );
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("o", ty::option(ty::int()))],
            ty::option(ty::int()),
            body,
        )]);
    }

    #[test]
    fn method_map_get_sugar_ok() {
        let body = expr::block(
            vec![LetStmt {
                pat: pat::bind("m"),
                ty: Some(ty::map(ty::string(), ty::int())),
                value: expr::map_lit(vec![(expr::str_("a"), expr::int(1))]),
            }],
            expr::method_call(expr::var("m"), "get", vec![expr::str_("a")]),
        );
        expect_ok(vec![decl::function("f", vec![], ty::option(ty::int()), body)]);
    }

    #[test]
    fn method_dispatches_to_function_typed_field_ok() {
        let rec_ty = ty::record(vec![("f", ty::fun(ty::int(), ty::int()))]);
        let body = expr::block(
            vec![LetStmt {
                pat: pat::bind("r"),
                ty: Some(rec_ty),
                value: expr::record_lit(vec![(
                    "f",
                    expr::lambda(
                        &[],
                        vec![pat::bind("x")],
                        expr::binop(BinOpKind::Add, expr::var("x"), expr::int(1)),
                    ),
                )]),
            }],
            expr::method_call(expr::var("r"), "f", vec![expr::int(1)]),
        );
        expect_ok(vec![decl::function("f", vec![], ty::int(), body)]);
    }

    #[test]
    fn method_user_function_shadows_stdlib_ok() {
        let size_fn = decl::function(
            "size",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            expr::int(0),
        );
        let body = expr::method_call(expr::var("xs"), "size", vec![]);
        let f = decl::function(
            "f",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            body,
        );
        expect_ok(vec![size_fn, f]);
    }

    #[test]
    fn method_unresolvable_errors() {
        let body = expr::method_call(expr::int(1), "nope", vec![]);
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), body)]);
        assert!(m.iter().any(|s| s.contains("cannot resolve method call `nope`")), "{m:?}");
    }

    // ---- records ------------------------------------------------------------------------

    #[test]
    fn record_update_ok() {
        let body = expr::record_upd(expr::var("u"), vec![("name", expr::str_("b"))]);
        expect_ok(vec![
            users_table(),
            decl::function(
                "f",
                vec![decl::param("u", ty::named("users"))],
                ty::named("users"),
                body,
            ),
        ]);
    }

    #[test]
    fn record_update_unknown_field_errors() {
        let body = expr::record_upd(expr::var("u"), vec![("nope", expr::int(1))]);
        let m = expect_err(vec![
            users_table(),
            decl::function(
                "f",
                vec![decl::param("u", ty::named("users"))],
                ty::named("users"),
                body,
            ),
        ]);
        assert!(m.iter().any(|s| s.contains("has no field `nope`")), "{m:?}");
    }

    #[test]
    fn record_update_wrong_type_errors() {
        let body = expr::record_upd(expr::var("u"), vec![("name", expr::int(1))]);
        let m = expect_err(vec![
            users_table(),
            decl::function(
                "f",
                vec![decl::param("u", ty::named("users"))],
                ty::named("users"),
                body,
            ),
        ]);
        assert!(
            m.iter().any(|s| s.contains("expected `string`, found `int`")),
            "{m:?}"
        );
    }

    #[test]
    fn field_access_and_tuple_projection() {
        let ok = decl::function(
            "f",
            vec![decl::param("p", ty::tuple(vec![ty::int(), ty::string()]))],
            ty::int(),
            expr::tuple_proj(expr::var("p"), 0),
        );
        expect_ok(vec![ok]);

        let bad_proj = expr::tuple_proj(expr::var("p"), 5);
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("p", ty::tuple(vec![ty::int(), ty::string()]))],
            ty::int(),
            bad_proj,
        )]);
        assert!(m.iter().any(|s| s.contains("has no component `5`")), "{m:?}");

        let bad_field = expr::field(expr::var("p"), "x");
        let m = expect_err(vec![decl::function(
            "f",
            vec![decl::param("p", ty::tuple(vec![ty::int(), ty::string()]))],
            ty::int(),
            bad_field,
        )]);
        assert!(m.iter().any(|s| s.contains("non-record type")), "{m:?}");
    }

    // ---- decimal / date / string literals ------------------------------------------------

    #[test]
    fn decimal_literal_precision_checked() {
        expect_ok(vec![decl::function(
            "f",
            vec![],
            ty::decimal(Some((5, 2))),
            expr::decimal("123.45", Some((5, 2))),
        )]);

        let m = expect_err(vec![decl::function(
            "g",
            vec![],
            ty::decimal(Some((4, 2))),
            expr::decimal("123.45", Some((4, 2))),
        )]);
        assert!(m.iter().any(|s| s.contains("significant digit")), "{m:?}");

        let m = expect_err(vec![decl::function(
            "h",
            vec![],
            ty::decimal(Some((5, 2))),
            expr::decimal("1.234", Some((5, 2))),
        )]);
        assert!(m.iter().any(|s| s.contains("fractional digit")), "{m:?}");
    }

    #[test]
    fn date_literal_validity_checked() {
        expect_ok(vec![decl::function("f", vec![], ty::date(), expr::date(2024, 2, 29))]);
        let m = expect_err(vec![decl::function("g", vec![], ty::date(), expr::date(2024, 2, 30))]);
        assert!(m.iter().any(|s| s.contains("invalid date literal")), "{m:?}");
    }

    #[test]
    fn string_interpolation_basic_types() {
        let ok = expr::str_interp(vec![
            StrPart::Lit("a ".into()),
            StrPart::Interp(expr::var("x")),
            StrPart::Lit(" b".into()),
        ]);
        expect_ok(vec![decl::function(
            "f",
            vec![decl::param("x", ty::int())],
            ty::string(),
            ok,
        )]);

        let bad = expr::str_interp(vec![StrPart::Interp(expr::var("u"))]);
        let m = expect_err(vec![
            users_table(),
            decl::function(
                "g",
                vec![decl::param("u", ty::named("users"))],
                ty::string(),
                bad,
            ),
        ]);
        assert!(m.iter().any(|s| s.contains("must have a basic type")), "{m:?}");
    }

    // ---- none / empty collections / misc inference ---------------------------------------

    #[test]
    fn none_needs_annotation() {
        let ok = decl::function("f", vec![], ty::option(ty::int()), expr::none());
        expect_ok(vec![ok]);

        let body = expr::block(
            vec![expr::let_(pat::bind("x"), expr::none())],
            expr::int(0),
        );
        let m = expect_err(vec![decl::function("g", vec![], ty::int(), body)]);
        assert!(m.iter().any(|s| s.contains("cannot infer the type of `none`")), "{m:?}");
    }

    #[test]
    fn empty_set_needs_annotation() {
        let body = expr::block(
            vec![expr::let_(pat::bind("s"), expr::set_lit(vec![]))],
            expr::int(0),
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), body)]);
        assert!(m.iter().any(|s| s.contains("empty `set {}`")), "{m:?}");
    }

    #[test]
    fn arithmetic_type_rules() {
        // decimal(m,n) is closed under arithmetic.
        let ok = expr::binop(
            BinOpKind::Add,
            expr::decimal("1.0", Some((5, 2))),
            expr::decimal("2.0", Some((5, 2))),
        );
        expect_ok(vec![decl::function("f", vec![], ty::decimal(Some((5, 2))), ok)]);

        let mixed = expr::binop(BinOpKind::Add, expr::int(1), expr::float(1.0));
        let m = expect_err(vec![decl::function("g", vec![], ty::int(), mixed)]);
        assert!(m.iter().any(|s| s.contains("same numeric type")), "{m:?}");

        let prec = expr::binop(
            BinOpKind::Add,
            expr::decimal("1.0", Some((5, 2))),
            expr::decimal("2.0", Some((6, 2))),
        );
        let m = expect_err(vec![decl::function("h", vec![], ty::decimal(Some((5, 2))), prec)]);
        assert!(m.iter().any(|s| s.contains("same numeric type")), "{m:?}");
    }

    #[test]
    fn membership_and_set_algebra() {
        let in_opt = decl::function(
            "f",
            vec![decl::param("o", ty::option(ty::int()))],
            ty::bool_(),
            expr::binop(BinOpKind::In, expr::int(1), expr::var("o")),
        );
        let cart = decl::function(
            "g",
            vec![
                decl::param("a", ty::set(ty::int())),
                decl::param("b", ty::set(ty::string())),
            ],
            ty::set(ty::tuple(vec![ty::int(), ty::string()])),
            expr::binop(BinOpKind::Cartesian, expr::var("a"), expr::var("b")),
        );
        let cup = decl::function(
            "h",
            vec![
                decl::param("a", ty::set(ty::int())),
                decl::param("b", ty::set(ty::int())),
            ],
            ty::set(ty::int()),
            expr::binop(BinOpKind::Cup, expr::var("a"), expr::var("b")),
        );
        expect_ok(vec![in_opt, cart, cup]);

        let bad = expr::binop(
            BinOpKind::In,
            expr::str_("a"),
            expr::set_lit(vec![expr::int(1)]),
        );
        let m = expect_err(vec![decl::function("k", vec![], ty::bool_(), bad)]);
        assert!(m.iter().any(|s| s.contains("expected `int`, found `string`")), "{m:?}");
    }

    #[test]
    fn enum_construction_payload_checked() {
        let ok = decl::function(
            "f",
            vec![],
            ty::named("shape"),
            expr::call("circle", vec![expr::float(1.0)]),
        );
        expect_ok(vec![shape_enum(), ok]);

        let bad = decl::function(
            "g",
            vec![],
            ty::named("shape"),
            expr::call("circle", vec![expr::int(1)]),
        );
        let m = expect_err(vec![shape_enum(), bad]);
        assert!(
            m.iter().any(|s| s.contains("expected `float`, found `int`")),
            "{m:?}"
        );
    }

    #[test]
    fn generic_enum_instantiated_from_expected_ok() {
        let result = decl::enum_gen(
            "result",
            &["T", "E"],
            vec![
                decl::variant_tuple("ok", vec![ty::named("T")]),
                decl::variant_tuple("err", vec![ty::named("E")]),
            ],
        );
        let ret = ty::t(TypeKind::Named {
            name: crate::ast::builder::id("result"),
            args: vec![ty::int(), ty::string()],
        });
        let f = decl::function("f", vec![], ret, expr::call("ok", vec![expr::int(1)]));
        let tm = expect_ok(vec![result, f]);
        assert_eq!(
            tm.operator_sigs["f"].ret,
            Ty::Enum { name: "result".into(), args: vec![Ty::Int, Ty::String] }
        );
    }

    #[test]
    fn let_annotation_mismatch_errors() {
        let body = expr::block(
            vec![LetStmt {
                pat: pat::bind("x"),
                ty: Some(ty::string()),
                value: expr::int(1),
            }],
            expr::int(0),
        );
        let m = expect_err(vec![decl::function("f", vec![], ty::int(), body)]);
        assert!(
            m.iter().any(|s| s.contains("expected `string`, found `int`")),
            "{m:?}"
        );
    }

    #[test]
    fn errors_are_isolated_per_operator() {
        // Two broken operators: both errors are reported (no abort).
        let bad1 = decl::function(
            "f",
            vec![],
            ty::int(),
            expr::binop(BinOpKind::Add, expr::int(1), expr::float(1.0)),
        );
        let bad2 = decl::function(
            "g",
            vec![],
            ty::string(),
            expr::cast(expr::int(1), ty::string()),
        );
        let (tm, bag) = check(vec![bad1, bad2]);
        assert_eq!(bag.error_count(), 2, "{}", bag.render());
        // Side tables are still produced for both operators.
        assert!(tm.operator_sigs.contains_key("f"));
        assert!(tm.operator_sigs.contains_key("g"));
    }
}
