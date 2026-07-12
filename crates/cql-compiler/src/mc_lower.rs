//! Model-checking lowering: desugared CQL core language → `cql-mc` [`McSpec`]
//! (`doc/model-check.md` §2.5, §4, §6.3).
//!
//! Lowering source: the **desugared AST** (`OptimizedModule.desugared`). At
//! that point `lookup` has become `only(read<T>(row.pk = k))`, quantifiers
//! have become `fold(to_vector(read<T>(true)), ...)` aggregates, and write
//! primitives are explicit [`WriteCon`] nodes — so the small v1 fragment maps
//! onto [`McExpr`] with almost no structural guesswork.
//!
//! The v1 checkable fragment is deliberately small (bool/int only, int-keyed
//! tables with a single int value field — the bank example of
//! `doc/model-check.md` §4.1 is the driving case). Anything outside the
//! fragment yields an "unsupported in mc fragment" diagnostic naming the
//! construct; lowering keeps going where possible so one run reports all
//! problems. Unsupported *properties* are skipped with a warning (other
//! properties still get checked); unsupported *tables/actions/expressions*
//! are errors (the model would be unsound without them).

use std::collections::HashMap;

use cql_mc::ir::{self, McExpr, McSpec, Property, PropertyKind, Transition, Update, UpdateKind};
use miette::NamedSource;

use crate::ast::{
    BinOpKind, EffectLevel, Expr, ExprKind, Item, Lambda, Literal, Module, Pattern, PatternKind,
    Span, TemporalExpr, TypeKind, UnOpKind, WriteCon,
};
use crate::diag::{CqlError, DiagBag};
use crate::project::ProjectOutput;

/// A finite domain bound from `verify.toml` `[domain]` (`doc/model-check.md` §6.3).
#[derive(Debug, Clone, PartialEq)]
pub enum DomainBound {
    /// `table.rows = N` — row-count bound for domain-generated initial
    /// states. (v1 lowers fixtures only; the bound is accepted and ignored.)
    Rows(usize),
    /// `table.field = [v1, v2, ...]` or `"lo..hi"` — explicit value set.
    Values(Vec<i64>),
}

impl DomainBound {
    /// Parse a `verify.toml` domain value: `"lo..hi"` range string, an integer
    /// array, or (for `.rows` keys) a plain integer.
    pub fn from_toml(key: &str, value: &toml::Value) -> Result<DomainBound, String> {
        match value {
            toml::Value::String(s) => {
                let (lo, hi) = s.split_once("..").ok_or_else(|| {
                    format!("domain `{key}`: expected \"lo..hi\" range, got `{s}`")
                })?;
                let lo: i64 = lo.trim().parse().map_err(|e| format!("domain `{key}`: {e}"))?;
                let hi: i64 = hi.trim().parse().map_err(|e| format!("domain `{key}`: {e}"))?;
                if lo > hi {
                    return Err(format!("domain `{key}`: empty range `{s}`"));
                }
                Ok(DomainBound::Values((lo..=hi).collect()))
            }
            toml::Value::Array(items) => {
                let mut out = Vec::new();
                for v in items {
                    match v {
                        toml::Value::Integer(i) => out.push(*i),
                        other => {
                            return Err(format!(
                                "domain `{key}`: only integer values are supported in the mc fragment, got {other}"
                            ))
                        }
                    }
                }
                Ok(DomainBound::Values(out))
            }
            toml::Value::Integer(i) if *i >= 0 => Ok(DomainBound::Rows(*i as usize)),
            other => Err(format!(
                "domain `{key}`: expected \"lo..hi\", an integer array, or a row count, got {other}"
            )),
        }
    }
}

/// Normalized verification configuration (from `verify.toml` + CLI overrides).
#[derive(Debug, Clone)]
pub struct McConfig {
    /// Default recursion/inlining depth bound (`[depth] default`).
    pub depth_default: u32,
    /// Per-operator depth overrides (`[depth] <op> = n`).
    pub depth_per_operator: HashMap<String, u32>,
    /// Domain bounds keyed by `"table.field"` or `"table.rows"`.
    pub domains: HashMap<String, DomainBound>,
    /// Trace length bound k (`[trace] length`).
    pub trace_length: u32,
    /// Weak fairness action names (`[fairness] weak`; accepted, not yet
    /// enforced by any backend — `doc/model-check.md` §4.3).
    pub fairness_weak: Vec<String>,
    /// Strong fairness action names (`[fairness] strong`).
    pub fairness_strong: Vec<String>,
}

impl Default for McConfig {
    fn default() -> Self {
        McConfig {
            depth_default: 32,
            depth_per_operator: HashMap::new(),
            domains: HashMap::new(),
            trace_length: 8,
            fairness_weak: Vec::new(),
            fairness_strong: Vec::new(),
        }
    }
}

/// Lower a compiled project to a checker-neutral [`McSpec`].
///
/// `sources` are the same `(label, text)` pairs passed to
/// [`crate::project::compile_project`]; they back diagnostic rendering.
/// Returns `Some(spec)` when lowering produced no errors.
pub fn lower_to_mc(
    project: &ProjectOutput,
    sources: &[(String, String)],
    config: &McConfig,
) -> (Option<McSpec>, DiagBag) {
    let mut bag = DiagBag::new();
    if project.modules.len() != 1 {
        let (label, text) = sources.first().map(|(l, t)| (l.clone(), t.clone())).unwrap_or_default();
        bag.push_error(CqlError::new(
            NamedSource::new(label, text),
            Span::new_dummy(),
            format!(
                "model checking supports single-module projects in v1 (found {} modules)",
                project.modules.len()
            ),
            Some("split off a single self-contained module to verify".to_string()),
        ));
        return (None, bag);
    }
    let compiled = &project.modules[0];
    let text = sources
        .iter()
        .find(|(l, _)| *l == compiled.label)
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let src = NamedSource::new(compiled.label.clone(), text);
    let module = &compiled.module.desugared.typed.resolved.module;
    let mut lw = Lowering::new(src, config);
    let spec = lw.lower_module(module);
    let bag = lw.bag;
    if bag.has_errors() {
        (None, bag)
    } else {
        (Some(spec), bag)
    }
}

// ---------------------------------------------------------------------------
// lowering state
// ---------------------------------------------------------------------------

/// A checkable table: single int primary key + single int value field.
struct TableInfo {
    key_field: String,
    value_field: String,
}

/// Value of an in-scope identifier during expression lowering.
#[derive(Clone)]
enum Binding {
    /// A pure int/bool value (action/operator parameter, let binding).
    Val(McExpr),
    /// A table row: field access on it lowers to `Select`/`key`.
    Row { table: usize, key: McExpr },
}

type Env = Vec<(String, Binding)>;

fn env_lookup<'a>(env: &'a Env, name: &str) -> Option<&'a Binding> {
    env.iter().rev().find(|(n, _)| n == name).map(|(_, b)| b)
}

struct Lowering<'a> {
    src: NamedSource<String>,
    config: &'a McConfig,
    bag: DiagBag,
    tables: Vec<TableInfo>,
    table_names: Vec<String>,
    /// Fixture rows collected from `test` blocks: (table, key, value).
    fixtures: Vec<(usize, i64, i64)>,
    /// Same-module operators by name (for inlining into properties/actions).
    operators: HashMap<String, &'a crate::ast::OperatorDecl>,
    /// Operator names currently being inlined (recursion detection).
    inlining: Vec<String>,
}

const FRAGMENT_HINT: &str = "the v1 mc fragment covers bool/int expressions over int-keyed tables \
     with a single int value field (doc/model-check.md §4)";

impl<'a> Lowering<'a> {
    fn new(src: NamedSource<String>, config: &'a McConfig) -> Self {
        Lowering {
            src,
            config,
            bag: DiagBag::new(),
            tables: Vec::new(),
            table_names: Vec::new(),
            fixtures: Vec::new(),
            operators: HashMap::new(),
            inlining: Vec::new(),
        }
    }

    fn unsupported(&mut self, span: Span, construct: impl std::fmt::Display) {
        self.bag.push_error(CqlError::new(
            self.src.clone(),
            span,
            format!("`{construct}` is not supported in the model-checking fragment"),
            Some(FRAGMENT_HINT.to_string()),
        ));
    }

    fn skip_warning(&mut self, span: Span, msg: impl Into<String>) {
        self.bag.push_warning(CqlError::new(
            self.src.clone(),
            span,
            msg,
            Some(FRAGMENT_HINT.to_string()),
        ));
    }

    fn table_index(&self, name: &str) -> Option<usize> {
        self.table_names.iter().position(|n| n == name)
    }

    // ---- top level ----

    fn lower_module(&mut self, module: &'a Module) -> McSpec {
        // 1. Tables.
        for item in &module.items {
            if let Item::Table(t) = item {
                self.lower_table(t);
            }
        }
        // 2. Operator index (for on-demand inlining).
        for item in &module.items {
            if let Item::Operator(op) = item {
                self.operators.insert(op.name.node.clone(), op);
            }
        }
        // 3. Fixtures from test blocks (initial state, §2.2).
        for item in &module.items {
            if let Item::Test(t) = item {
                self.lower_fixtures(t);
            }
        }
        // 4. Actions → transitions.
        let mut transitions = Vec::new();
        for item in &module.items {
            if let Item::Operator(op) = item {
                if op.level == EffectLevel::Action {
                    if let Some(tr) = self.lower_action(op) {
                        transitions.push(tr);
                    }
                }
            }
        }
        // 5. Invariants + properties.
        let mut properties = Vec::new();
        for item in &module.items {
            match item {
                Item::Invariant(inv) => {
                    let mut env = Vec::new();
                    if let Some(e) = self.lower_expr(&inv.body, &mut env) {
                        properties.push(Property {
                            name: inv.name.node.clone(),
                            kind: PropertyKind::Always(e),
                        });
                    }
                }
                Item::Property(p) => self.lower_property(p, &mut properties),
                _ => {}
            }
        }
        McSpec {
            tables: self
                .table_names
                .iter()
                .map(|n| ir::TableDecl { name: n.clone() })
                .collect(),
            init: self.fixtures.clone(),
            transitions,
            properties,
            depth: self.config.trace_length,
        }
    }

    fn lower_table(&mut self, t: &'a crate::ast::TableDecl) {
        let fail = |lw: &mut Self, msg: String| {
            lw.bag.push_error(CqlError::new(
                lw.src.clone(),
                t.name.span,
                format!("table `{}` is not checkable: {msg}", t.name.node),
                Some(FRAGMENT_HINT.to_string()),
            ));
        };
        if t.pk.len() != 1 {
            fail(self, "primary key must be a single field".to_string());
            return;
        }
        let pk = &t.pk[0].node;
        let field_ty = |name: &str| {
            t.fields
                .iter()
                .find(|(f, _)| f.node == name)
                .map(|(_, ty)| &ty.kind)
        };
        if field_ty(pk) != Some(&TypeKind::Int) {
            fail(self, format!("primary key `{pk}` must have type `int`"));
            return;
        }
        let int_fields: Vec<&str> = t
            .fields
            .iter()
            .filter(|(f, ty)| f.node != *pk && ty.kind == TypeKind::Int)
            .map(|(f, _)| f.node.as_str())
            .collect();
        if int_fields.len() != 1 {
            fail(
                self,
                format!(
                    "expected exactly one non-key `int` field (the modeled value), found {}",
                    int_fields.len()
                ),
            );
            return;
        }
        let ignored: Vec<&str> = t
            .fields
            .iter()
            .filter(|(f, ty)| f.node != *pk && ty.kind != TypeKind::Int)
            .map(|(f, _)| f.node.as_str())
            .collect();
        if !ignored.is_empty() {
            self.bag.push_warning(CqlError::new(
                self.src.clone(),
                t.name.span,
                format!(
                    "table `{}`: non-int field(s) {} are not part of the model and are ignored",
                    t.name.node,
                    ignored.join(", ")
                ),
                Some("the mc fragment models tables as int key ⇀ int value maps".to_string()),
            ));
        }
        self.table_names.push(t.name.node.clone());
        self.tables.push(TableInfo {
            key_field: pk.clone(),
            value_field: int_fields[0].to_string(),
        });
    }

    fn lower_fixtures(&mut self, t: &crate::ast::TestDecl) {
        for stmt in &t.stmts {
            let crate::ast::TestStmt::Fixture { table, rows } = stmt else {
                continue;
            };
            let Some(ti) = self.table_index(&table.node) else {
                // Fixture for an uncheckable table: table lowering already errored.
                continue;
            };
            let ExprKind::Vector(row_exprs) = &rows.kind else {
                self.unsupported(rows.span, "non-literal fixture rows");
                continue;
            };
            for row in row_exprs {
                let ExprKind::RecordLit { fields } = &row.kind else {
                    self.unsupported(row.span, "non-record fixture row");
                    continue;
                };
                let get_int = |name: &str| {
                    fields.iter().find(|f| f.name.node == name).and_then(|f| match &f.value.kind {
                        ExprKind::Lit(Literal::Int(i)) => Some(*i),
                        _ => None,
                    })
                };
                let info = &self.tables[ti];
                match (get_int(&info.key_field), get_int(&info.value_field)) {
                    (Some(k), Some(v)) => {
                        if !self.fixtures.iter().any(|(t, k2, _)| *t == ti && *k2 == k) {
                            self.fixtures.push((ti, k, v));
                        }
                    }
                    _ => self.unsupported(row.span, "fixture row with non-int key/value"),
                }
            }
        }
    }

    // ---- actions ----

    fn lower_action(&mut self, op: &'a crate::ast::OperatorDecl) -> Option<Transition> {
        let start_errors = self.bag.error_count();
        if !op.type_params.is_empty() {
            self.unsupported(op.name.span, "generic action");
            return None;
        }
        if op.recursive || op.depth.is_some() || op.decreases.is_some() {
            self.unsupported(op.name.span, "recursive action");
            return None;
        }
        let mut params = Vec::new();
        let mut env: Env = Vec::new();
        for (i, p) in op.params.iter().enumerate() {
            match p.ty.kind {
                TypeKind::Int => {
                    params.push(ir::Ty::Int);
                    env.push((p.name.node.clone(), Binding::Val(ir::param(i))));
                }
                _ => {
                    self.unsupported(p.ty.span, format!("action parameter of type `{}`", ty_name(&p.ty.kind)));
                }
            }
        }
        let domains = self.infer_param_domains(op);
        let body = op.body.as_ref()?;
        let mut acc = ActionAcc::default();
        self.collect_body(body, &mut env, &mut acc);
        if self.bag.error_count() > start_errors {
            return None;
        }
        Some(Transition {
            name: op.name.node.clone(),
            params,
            param_domains: domains,
            guard: ir::and(acc.guards),
            updates: acc.updates,
        })
    }

    /// Infer a finite domain per parameter (§2.3: bounded parameter domains):
    /// key-usage params take the table's key domain, params compared/combined
    /// with a table's value field take that field's domain, others get a small
    /// default domain.
    fn infer_param_domains(&mut self, op: &crate::ast::OperatorDecl) -> Vec<Vec<i64>> {
        let mut roles: Vec<DomainRole> = op.params.iter().map(|_| DomainRole::None).collect();
        if let Some(body) = &op.body {
            let names: Vec<&str> = op.params.iter().map(|p| p.name.node.as_str()).collect();
            walk_domain_roles(body, &names, &mut roles, self);
        }
        roles
            .iter()
            .enumerate()
            .map(|(i, role)| match role {
                DomainRole::KeyOf(t) => self.key_domain(*t),
                DomainRole::ValueFieldOf(t) => {
                    let tname = self.table_names[*t].clone();
                    let vfield = self.tables[*t].value_field.clone();
                    match self.config.domains.get(&format!("{tname}.{vfield}")) {
                        Some(DomainBound::Values(vs)) if !vs.is_empty() => vs.clone(),
                        _ => {
                            self.bag.push_warning(CqlError::new(
                                self.src.clone(),
                                op.params[i].name.span,
                                format!(
                                    "no domain bound for parameter `{}`; using default [-1, 0, 1] \
                                     (add `{tname}.{vfield}` to verify.toml [domain])",
                                    op.params[i].name.node
                                ),
                                None,
                            ));
                            vec![-1, 0, 1]
                        }
                    }
                }
                DomainRole::None => {
                    self.bag.push_warning(CqlError::new(
                        self.src.clone(),
                        op.params[i].name.span,
                        format!(
                            "no domain bound for parameter `{}`; using default [-1, 0, 1]",
                            op.params[i].name.node
                        ),
                        None,
                    ));
                    vec![-1, 0, 1]
                }
            })
            .collect()
    }

    /// Finite key domain of a table: `verify.toml` `table.<key>` ∪ fixture keys.
    fn key_domain(&self, t: usize) -> Vec<i64> {
        let info = &self.tables[t];
        let mut keys: Vec<i64> = match self
            .config
            .domains
            .get(&format!("{}.{}", self.table_names[t], info.key_field))
        {
            Some(DomainBound::Values(vs)) => vs.clone(),
            _ => Vec::new(),
        };
        for (ft, k, _) in &self.fixtures {
            if *ft == t {
                keys.push(*k);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        if keys.is_empty() {
            // Last resort: a trivial singleton domain keeps the model finite.
            keys.push(0);
        }
        keys
    }

    // ---- action body: guards + writes ----

    fn collect_body(&mut self, e: &Expr, env: &mut Env, acc: &mut ActionAcc) {
        match &e.kind {
            ExprKind::SetLiteral(elems) => {
                for el in elems {
                    self.lower_write(el, env, acc);
                }
            }
            ExprKind::If { cond, then_br, else_br } => {
                if !is_empty_set(else_br) {
                    self.unsupported(e.span, "conditional write set with non-empty `else` branch");
                    return;
                }
                if let Some(g) = self.lower_expr(cond, env) {
                    acc.guards.push(g);
                }
                self.collect_body(then_br, env, acc);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_match(scrutinee, arms, env, acc, e.span);
            }
            ExprKind::Let { pat, value, body } => {
                let PatternKind::Bind(id) = &pat.kind else {
                    self.unsupported(pat.span, "destructuring let in action body");
                    return;
                };
                match self.lower_expr(value, env) {
                    Some(v) => {
                        env.push((id.node.clone(), Binding::Val(v)));
                        self.collect_body(body, env, acc);
                        env.pop();
                    }
                    None => self.collect_body(body, env, acc),
                }
            }
            _ => self.unsupported(e.span, format!("action body construct `{}`", kind_name(e))),
        }
    }

    fn collect_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::MatchArm],
        env: &mut Env,
        acc: &mut ActionAcc,
        span: Span,
    ) {
        // v1: match on a tuple of `lookup` results (or a single lookup) with
        // one all-`Some` arm and empty-set fallback arms.
        let lookups = match self.scrutinee_lookups(scrutinee) {
            Some(ls) => ls,
            None => {
                self.unsupported(span, "match scrutinee (only `match lookup(t, k)` is supported)");
                return;
            }
        };
        for arm in arms {
            // Fallback arm (`_` / partial wildcard): must produce no writes.
            if matches!(&arm.pat.kind, PatternKind::Wildcard) {
                if !is_empty_set(&arm.body) {
                    self.unsupported(arm.body.span, "non-empty fallback arm of lookup match");
                }
                continue;
            }
            let pats: Vec<&Pattern> = match &arm.pat.kind {
                PatternKind::Tuple(ps) if lookups.len() > 1 => ps.iter().collect(),
                _ if lookups.len() == 1 => vec![&arm.pat],
                _ => {
                    self.unsupported(arm.pat.span, "match arm pattern in mc fragment");
                    continue;
                }
            };
            if pats.iter().any(|p| matches!(p.kind, PatternKind::Wildcard)) {
                if !is_empty_set(&arm.body) {
                    self.unsupported(arm.body.span, "non-empty fallback arm of lookup match");
                }
                continue;
            }
            // All-Some arm.
            let mut bindings = Vec::new();
            let mut ok = true;
            for (pat, (t, key)) in pats.iter().zip(&lookups) {
                let PatternKind::Some(inner) = &pat.kind else {
                    self.unsupported(pat.span, "match arm pattern (expected `some(x)`)");
                    ok = false;
                    break;
                };
                let PatternKind::Bind(id) = &inner.kind else {
                    self.unsupported(inner.span, "nested pattern inside `some(...)`");
                    ok = false;
                    break;
                };
                bindings.push((id.node.clone(), *t, key));
            }
            if !ok {
                continue;
            }
            // Guards: every looked-up key must be present.
            for (_, t, key) in &bindings {
                if let Some(k) = self.lower_expr(key, env) {
                    acc.guards.push(ir::contains(*t, k));
                }
            }
            // Row bindings for the arm body.
            let mut pushed = 0;
            for (name, t, key) in &bindings {
                if let Some(k) = self.lower_expr(key, env) {
                    env.push((name.clone(), Binding::Row { table: *t, key: k }));
                    pushed += 1;
                }
            }
            self.collect_body(&arm.body, env, acc);
            for _ in 0..pushed {
                env.pop();
            }
        }
    }

    /// Recognize `lookup` scrutinee shapes: the desugared
    /// `let k = key in only(read<T>(row.pk = k))` (possibly in a tuple), or a
    /// direct `lookup(T, key)` call.
    fn scrutinee_lookups<'e>(&mut self, e: &'e Expr) -> Option<Vec<(usize, &'e Expr)>> {
        let one = |e: &'e Expr| -> Option<(usize, &'e Expr)> {
            // Desugared: Let(k = key, Call(only, [ReadPrim{table, lambda row: row.pk == k}]))
            if let ExprKind::Let { pat, value, body } = &e.kind {
                let PatternKind::Bind(kv) = &pat.kind else { return None };
                let ExprKind::Call(call) = &body.kind else { return None };
                if call.name.node != "only" || call.args.len() != 1 {
                    return None;
                }
                let ExprKind::ReadPrim { table, predicate } = &call.args[0].value.kind else {
                    return None;
                };
                if !is_key_predicate(predicate, &kv.node) {
                    return None;
                }
                let t = self.table_index(&table.node)?;
                return Some((t, value));
            }
            // Surface form (kept by resolve): lookup(T, key).
            if let ExprKind::Call(call) = &e.kind {
                if call.name.node == "lookup" && call.args.len() == 2 {
                    if let ExprKind::Var(tname) = &call.args[0].value.kind {
                        let t = self.table_index(&tname.node)?;
                        return Some((t, &call.args[1].value));
                    }
                }
            }
            None
        };
        match &e.kind {
            ExprKind::Tuple(es) => es.iter().map(one).collect(),
            _ => one(e).map(|x| vec![x]),
        }
    }

    fn lower_write(&mut self, e: &Expr, env: &mut Env, acc: &mut ActionAcc) {
        let ExprKind::WriteCon(w) = &e.kind else {
            self.unsupported(e.span, format!("write-set element `{}`", kind_name(e)));
            return;
        };
        match w {
            WriteCon::Insert { table, row } => {
                let Some(t) = self.table_index(&table.node) else {
                    self.unsupported(table.span, format!("insert into uncheckable table `{}`", table.node));
                    return;
                };
                let key_field = self.tables[t].key_field.clone();
                let value_field = self.tables[t].value_field.clone();
                let ExprKind::RecordLit { fields } = &row.kind else {
                    self.unsupported(row.span, "insert with non-record row");
                    return;
                };
                let mut key = None;
                let mut value = None;
                for f in fields {
                    if f.name.node == key_field {
                        key = self.lower_expr(&f.value, env);
                    } else if f.name.node == value_field {
                        value = self.lower_expr(&f.value, env);
                    }
                }
                match (key, value) {
                    (Some(k), Some(v)) => acc.updates.push(Update {
                        table: t,
                        key: k,
                        kind: UpdateKind::Insert,
                        value: Some(v),
                    }),
                    _ => self.unsupported(row.span, "insert row missing key or value field"),
                }
            }
            WriteCon::Update { table, key, transform } => {
                let Some(t) = self.table_index(&table.node) else {
                    self.unsupported(table.span, format!("update of uncheckable table `{}`", table.node));
                    return;
                };
                let Some(k) = self.lower_expr(key, env) else { return };
                let ExprKind::Lambda(lam) = &transform.kind else {
                    self.unsupported(transform.span, "update transform must be a lambda");
                    return;
                };
                if lam.params.len() != 1 {
                    self.unsupported(transform.span, "update transform must take one row parameter");
                    return;
                }
                let PatternKind::Bind(v) = &lam.params[0].pat.kind else {
                    self.unsupported(lam.params[0].pat.span, "update transform parameter pattern");
                    return;
                };
                let ExprKind::RecordUpd { base, fields } = &lam.body.kind else {
                    self.unsupported(lam.body.span, "update transform body must be a record update");
                    return;
                };
                if !matches!(&base.kind, ExprKind::Var(b) if b.node == v.node) {
                    self.unsupported(base.span, "record update base must be the row parameter");
                    return;
                }
                let value_field = self.tables[t].value_field.clone();
                let mut value = None;
                for f in fields {
                    if f.name.node == value_field {
                        env.push((v.node.clone(), Binding::Row { table: t, key: k.clone() }));
                        value = self.lower_expr(&f.value, env);
                        env.pop();
                    }
                    // Non-value fields are unchanged / not modeled.
                }
                match value {
                    Some(vv) => acc.updates.push(Update {
                        table: t,
                        key: k,
                        kind: UpdateKind::Update,
                        value: Some(vv),
                    }),
                    None => self.unsupported(lam.body.span, "update transform does not set the modeled int field"),
                }
            }
            WriteCon::Delete { table, key } => {
                let Some(t) = self.table_index(&table.node) else {
                    self.unsupported(table.span, format!("delete from uncheckable table `{}`", table.node));
                    return;
                };
                if let Some(k) = self.lower_expr(key, env) {
                    acc.updates.push(Update {
                        table: t,
                        key: k,
                        kind: UpdateKind::Delete,
                        value: None,
                    });
                }
            }
        }
    }

    // ---- properties ----

    fn lower_property(&mut self, p: &crate::ast::PropertyDecl, out: &mut Vec<Property>) {
        let kind = match &p.body {
            TemporalExpr::Always(inner) => {
                let TemporalExpr::State(pred) = &**inner else {
                    self.skip_warning(
                        p.name.span,
                        format!("property `{}`: nested temporal operators are not supported; skipped", p.name.node),
                    );
                    return;
                };
                if has_primed(pred) {
                    self.skip_warning(
                        p.name.span,
                        format!(
                            "property `{}`: prime (next-state) is not supported by the stateright backend; skipped",
                            p.name.node
                        ),
                    );
                    return;
                }
                let mut env = Vec::new();
                match self.lower_expr(pred, &mut env) {
                    Some(e) => PropertyKind::Always(e),
                    None => return,
                }
            }
            TemporalExpr::State(pred) => {
                if has_primed(pred) {
                    self.skip_warning(
                        p.name.span,
                        format!(
                            "property `{}`: prime (next-state) is not supported by the stateright backend; skipped",
                            p.name.node
                        ),
                    );
                    return;
                }
                let mut env = Vec::new();
                match self.lower_expr(pred, &mut env) {
                    Some(e) => PropertyKind::Always(e),
                    None => return,
                }
            }
            TemporalExpr::Eventually(inner) => {
                let TemporalExpr::State(pred) = &**inner else {
                    self.skip_warning(
                        p.name.span,
                        format!("property `{}`: nested temporal operators are not supported; skipped", p.name.node),
                    );
                    return;
                };
                if has_primed(pred) {
                    self.skip_warning(
                        p.name.span,
                        format!("property `{}`: prime is not supported; skipped", p.name.node),
                    );
                    return;
                }
                let mut env = Vec::new();
                match self.lower_expr(pred, &mut env) {
                    Some(e) => PropertyKind::Eventually(e),
                    None => return,
                }
            }
            TemporalExpr::LeadsTo { .. } | TemporalExpr::Until { .. } => {
                self.skip_warning(
                    p.name.span,
                    format!(
                        "property `{}`: `~>`/`until` are not supported by the stateright backend; skipped",
                        p.name.node
                    ),
                );
                return;
            }
            TemporalExpr::Primed(_) => {
                self.skip_warning(
                    p.name.span,
                    format!("property `{}`: bare prime is not supported; skipped", p.name.node),
                );
                return;
            }
        };
        out.push(Property {
            name: p.name.node.clone(),
            kind,
        });
    }

    // ---- expressions ----

    fn lower_expr(&mut self, e: &Expr, env: &mut Env) -> Option<McExpr> {
        match &e.kind {
            ExprKind::Lit(l) => match l {
                Literal::Int(i) => Some(ir::int(*i)),
                Literal::Bool(b) => Some(ir::bool_(*b)),
                other => {
                    self.unsupported(e.span, format!("literal `{other:?}`"));
                    None
                }
            },
            ExprKind::Var(id) => match env_lookup(env, &id.node) {
                Some(Binding::Val(v)) => Some(v.clone()),
                Some(Binding::Row { .. }) => {
                    self.unsupported(e.span, format!("row `{id}` used as a value (use `{id}.<field>`)", id = id.node));
                    None
                }
                None => {
                    if self.table_index(&id.node).is_some() {
                        self.unsupported(e.span, format!("table `{id}` in value position", id = id.node));
                    } else {
                        self.unsupported(e.span, format!("unresolved name `{id}`", id = id.node));
                    }
                    None
                }
            },
            ExprKind::Field { base, name } => {
                let ExprKind::Var(id) = &base.kind else {
                    self.unsupported(base.span, "field access on a non-row expression");
                    return None;
                };
                match env_lookup(env, &id.node) {
                    Some(Binding::Row { table, key }) => {
                        let info = &self.tables[*table];
                        if name.node == info.value_field {
                            Some(ir::select(*table, key.clone()))
                        } else if name.node == info.key_field {
                            Some(key.clone())
                        } else {
                            self.unsupported(
                                e.span,
                                format!("field `{n}` is not part of the model", n = name.node),
                            );
                            None
                        }
                    }
                    _ => {
                        self.unsupported(base.span, format!("`{id}` is not a table row", id = id.node));
                        None
                    }
                }
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr(lhs, env)?;
                let r = self.lower_expr(rhs, env)?;
                Some(match op {
                    BinOpKind::Add => ir::add(l, r),
                    BinOpKind::Sub => ir::sub(l, r),
                    BinOpKind::Mul => ir::mul(l, r),
                    BinOpKind::Eq => ir::eq(l, r),
                    BinOpKind::Ne => ir::ne(l, r),
                    BinOpKind::Lt => ir::lt(l, r),
                    BinOpKind::Le => ir::le(l, r),
                    BinOpKind::Gt => ir::gt(l, r),
                    BinOpKind::Ge => ir::ge(l, r),
                    BinOpKind::And => ir::and(vec![l, r]),
                    BinOpKind::Or => ir::or(vec![l, r]),
                    BinOpKind::Impl => ir::implies(l, r),
                    other => {
                        self.unsupported(e.span, format!("operator `{other:?}`"));
                        return None;
                    }
                })
            }
            ExprKind::UnOp { op, operand } => {
                let x = self.lower_expr(operand, env)?;
                Some(match op {
                    UnOpKind::Not => ir::not(x),
                    UnOpKind::Neg => ir::sub(ir::int(0), x),
                })
            }
            ExprKind::Let { pat, value, body } => {
                let PatternKind::Bind(id) = &pat.kind else {
                    self.unsupported(pat.span, "destructuring let");
                    return None;
                };
                let v = self.lower_expr(value, env)?;
                env.push((id.node.clone(), Binding::Val(v)));
                let r = self.lower_expr(body, env);
                env.pop();
                r
            }
            ExprKind::Call(call) => self.lower_call(&call.name.node, &call.args, env, e.span),
            ExprKind::Primed(_) => {
                self.unsupported(e.span, "prime (next-state) expression");
                None
            }
            _ => {
                self.unsupported(e.span, format!("expression `{}`", kind_name(e)));
                None
            }
        }
    }

    fn lower_call(
        &mut self,
        name: &str,
        args: &[crate::ast::Arg],
        env: &mut Env,
        span: Span,
    ) -> Option<McExpr> {
        // Aggregates: fold(to_vector(read<T>(true)), init, step).
        if name == "fold" && args.len() == 3 {
            return self.lower_fold(&args[0].value, &args[1].value, &args[2].value, env, span);
        }
        // User-defined function/query: inline (D-bounded, §3.2).
        let Some(op) = self.operators.get(name).copied() else {
            self.unsupported(span, format!("call to `{name}`"));
            return None;
        };
        if op.level == EffectLevel::Action {
            self.unsupported(span, format!("call to action `{name}`"));
            return None;
        }
        if !op.type_params.is_empty() {
            self.unsupported(span, format!("generic operator `{name}`"));
            return None;
        }
        let limit = self
            .config
            .depth_per_operator
            .get(name)
            .copied()
            .unwrap_or(self.config.depth_default) as usize;
        if self.inlining.iter().filter(|n| n.as_str() == name).count() >= limit
            || self.inlining.contains(&name.to_string()) && op.recursive
        {
            self.unsupported(
                span,
                format!("recursive operator `{name}` (depth bound {limit} exceeded)"),
            );
            return None;
        }
        if args.len() != op.params.len() {
            self.unsupported(span, format!("call to `{name}` with wrong number of arguments"));
            return None;
        }
        let mut call_env: Env = Vec::new();
        for (i, param) in op.params.iter().enumerate() {
            let arg = args
                .iter()
                .enumerate()
                .find(|(j, a)| a.name.as_ref().map(|n| n.node.as_str()) == Some(param.name.node.as_str()) || *j == i && a.name.is_none())
                .map(|(_, a)| a)
                .unwrap_or(&args[i]);
            match param.ty.kind {
                TypeKind::Int | TypeKind::Bool => {
                    let v = self.lower_expr(&arg.value, env)?;
                    call_env.push((param.name.node.clone(), Binding::Val(v)));
                }
                _ => {
                    self.unsupported(
                        param.ty.span,
                        format!("operator parameter of type `{}`", ty_name(&param.ty.kind)),
                    );
                    return None;
                }
            }
        }
        self.inlining.push(name.to_string());
        let r = self.lower_expr(op.body.as_ref()?, &mut call_env);
        self.inlining.pop();
        r
    }

    /// `fold(to_vector(read<T>(true)), init, lambda(acc, x) { acc ⊕ term })`:
    /// sum / forall / exists over the table's finite key domain.
    fn lower_fold(
        &mut self,
        source: &Expr,
        init: &Expr,
        step: &Expr,
        env: &mut Env,
        span: Span,
    ) -> Option<McExpr> {
        // Source: to_vector(read<T>(λ row. true)) or read<T>(λ row. true).
        let mut src = source;
        if let ExprKind::Call(c) = &src.kind {
            if c.name.node == "to_vector" && c.args.len() == 1 {
                src = &c.args[0].value;
            }
        }
        let ExprKind::ReadPrim { table, predicate } = &src.kind else {
            self.unsupported(span, "fold source (only `fold(to_vector(table), ...)` is supported)");
            return None;
        };
        if !is_true_predicate(predicate) {
            self.unsupported(predicate.span, "filtered read in an aggregate (predicate must be `true`)");
            return None;
        }
        let Some(t) = self.table_index(&table.node) else {
            self.unsupported(table.span, format!("aggregate over uncheckable table `{}`", table.node));
            return None;
        };
        let ExprKind::Lambda(Lambda { params, body, .. }) = &step.kind else {
            self.unsupported(step.span, "fold step must be a lambda");
            return None;
        };
        if params.len() != 2 {
            self.unsupported(step.span, "fold step must take (acc, element)");
            return None;
        }
        let (PatternKind::Bind(acc_id), PatternKind::Bind(x_id)) =
            (&params[0].pat.kind, &params[1].pat.kind)
        else {
            self.unsupported(step.span, "fold step parameter patterns");
            return None;
        };
        let domain = self.key_domain(t);
        // Peel `acc ⊕ term`.
        let ExprKind::BinOp { op, lhs, rhs } = &body.kind else {
            self.unsupported(body.span, "fold step body (expected `acc + term` / `acc and term`)");
            return None;
        };
        let (term, is_acc_lhs) = if matches!(&lhs.kind, ExprKind::Var(v) if v.node == acc_id.node) {
            (rhs, true)
        } else if matches!(&rhs.kind, ExprKind::Var(v) if v.node == acc_id.node) {
            (lhs, false)
        } else {
            self.unsupported(body.span, "fold step body must combine `acc` with a term");
            return None;
        };
        let _ = is_acc_lhs;
        match (op, &init.kind) {
            // Sum: init int, term = value-field access.
            (BinOpKind::Add, ExprKind::Lit(Literal::Int(i0))) => {
                let is_value_field = matches!(&term.kind, ExprKind::Field { base, name }
                    if matches!(&base.kind, ExprKind::Var(v) if v.node == x_id.node)
                        && name.node == self.tables[t].value_field);
                if !is_value_field {
                    self.unsupported(term.span, "sum term (only sums of the table's int value field are supported)");
                    return None;
                }
                let s = ir::sum(t, domain);
                Some(if *i0 == 0 { s } else { ir::add(ir::int(*i0), s) })
            }
            // Forall: init true, body acc && P(x) → ⋀ k∈dom. contains ⇒ P[x := row@k].
            (BinOpKind::And, ExprKind::Lit(Literal::Bool(true))) => {
                let mut conjuncts = Vec::new();
                for k in domain {
                    env.push((
                        x_id.node.clone(),
                        Binding::Row { table: t, key: ir::int(k) },
                    ));
                    let p = self.lower_expr(term, env);
                    env.pop();
                    conjuncts.push(ir::implies(ir::contains(t, ir::int(k)), p?));
                }
                Some(ir::and(conjuncts))
            }
            // Exists: init false, body acc || P(x) → ⋁ k∈dom. contains ∧ P.
            (BinOpKind::Or, ExprKind::Lit(Literal::Bool(false))) => {
                let mut disjuncts = Vec::new();
                for k in domain {
                    env.push((
                        x_id.node.clone(),
                        Binding::Row { table: t, key: ir::int(k) },
                    ));
                    let p = self.lower_expr(term, env);
                    env.pop();
                    disjuncts.push(ir::and(vec![ir::contains(t, ir::int(k)), p?]));
                }
                Some(ir::or(disjuncts))
            }
            _ => {
                self.unsupported(span, "fold shape (supported: sum, forall `and`, exists `or`)");
                None
            }
        }
    }
}

#[derive(Default)]
struct ActionAcc {
    guards: Vec<McExpr>,
    updates: Vec<Update>,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum DomainRole {
    None,
    KeyOf(usize),
    ValueFieldOf(usize),
}

/// Walk the raw action body once, classifying each parameter's domain role:
/// used as a table key (strongest), used with a table's value field, or none.
fn walk_domain_roles(e: &Expr, params: &[&str], roles: &mut [DomainRole], lw: &Lowering) {
    let mark = |ex: &Expr, role: DomainRole, roles: &mut [DomainRole]| {
        if let ExprKind::Var(v) = &ex.kind {
            if let Some(i) = params.iter().position(|p| *p == v.node) {
                match (roles[i], role) {
                    (DomainRole::KeyOf(_), _) => {}
                    (_, DomainRole::KeyOf(_)) => roles[i] = role,
                    (DomainRole::None, r) => roles[i] = r,
                    _ => {}
                }
            }
        }
    };
    // Key usage: WriteCon keys and desugared lookup `let k = <param> in only(read..)`.
    match &e.kind {
        ExprKind::WriteCon(w) => match w {
            WriteCon::Insert { table, row } => {
                if let Some(t) = lw.table_index(&table.node) {
                    if let ExprKind::RecordLit { fields } = &row.kind {
                        let kf = lw.tables[t].key_field.clone();
                        let vf = lw.tables[t].value_field.clone();
                        for f in fields {
                            if f.name.node == kf {
                                mark(&f.value, DomainRole::KeyOf(t), roles);
                            } else if f.name.node == vf {
                                mark(&f.value, DomainRole::ValueFieldOf(t), roles);
                            }
                        }
                    }
                }
            }
            WriteCon::Update { table, key, .. } | WriteCon::Delete { table, key } => {
                if let Some(t) = lw.table_index(&table.node) {
                    mark(key, DomainRole::KeyOf(t), roles);
                }
            }
        },
        ExprKind::Let { pat, value, body } => {
            if let PatternKind::Bind(kv) = &pat.kind {
                if let ExprKind::Call(c) = &body.kind {
                    if c.name.node == "only" {
                        if let Some(ExprKind::ReadPrim { table, predicate }) =
                            c.args.first().map(|a| &a.value.kind)
                        {
                            if is_key_predicate(predicate, &kv.node) {
                                if let Some(t) = lw.table_index(&table.node) {
                                    mark(value, DomainRole::KeyOf(t), roles);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Value-field usage: `param` directly compared/combined with `row.<valuefield>`.
        ExprKind::BinOp { lhs, rhs, .. } => {
            for (a, b) in [(lhs, rhs), (rhs, lhs)] {
                if let ExprKind::Field { base, name } = &b.kind {
                    if let ExprKind::Var(_) = &base.kind {
                        for (t, info) in lw.tables.iter().enumerate() {
                            if name.node == info.value_field {
                                mark(a, DomainRole::ValueFieldOf(t), roles);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    // Recurse over all child expressions.
    for child in child_exprs(e) {
        walk_domain_roles(child, params, roles, lw);
    }
}

fn child_exprs(e: &Expr) -> Vec<&Expr> {
    use ExprKind::*;
    let mut out: Vec<&Expr> = Vec::new();
    match &e.kind {
        Block { lets, tail } => {
            out.extend(lets.iter().map(|l| &l.value));
            out.push(tail);
        }
        Let { value, body, .. } => {
            out.push(value);
            out.push(body);
        }
        Lambda(l) => out.push(&l.body),
        App { func, args } => {
            out.push(func);
            out.extend(args.iter().map(|a| &a.value));
        }
        Call(c) => out.extend(c.args.iter().map(|a| &a.value)),
        Match { scrutinee, arms } => {
            out.push(scrutinee);
            out.extend(arms.iter().map(|a| &a.body));
        }
        If { cond, then_br, else_br } => {
            out.push(cond);
            out.push(then_br);
            out.push(else_br);
        }
        Try(x) | OptionSome(x) | Primed(x) | Cast { expr: x, .. } => out.push(x),
        RecordLit { fields } => out.extend(fields.iter().map(|f| &f.value)),
        RecordUpd { base, fields } => {
            out.push(base);
            out.extend(fields.iter().map(|f| &f.value));
        }
        Tuple(v) | Vector(v) | SetLiteral(v) | BagLiteral(v) => out.extend(v.iter()),
        SetFilter { source, pred, .. } => {
            out.push(source);
            out.push(pred);
        }
        SetMap { elem, gens } | BagMap { elem, gens } => {
            out.push(elem);
            out.extend(gens.iter().map(|g| &g.source));
        }
        MapLit(v) => {
            for (k, val) in v {
                out.push(k);
                out.push(val);
            }
        }
        Quantifier { gens, body, .. } => {
            out.extend(gens.iter().map(|g| &g.source));
            out.push(body);
        }
        BinOp { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        UnOp { operand, .. } => out.push(operand),
        Field { base, .. } | TupleProj { base, .. } => out.push(base),
        MethodCall { recv, args, .. } => {
            out.push(recv);
            out.extend(args.iter().map(|a| &a.value));
        }
        ReadPrim { predicate, .. } => out.push(predicate),
        WriteCon(w) => match w {
            crate::ast::WriteCon::Insert { row, .. } => out.push(row),
            crate::ast::WriteCon::Update { key, transform, .. } => {
                out.push(key);
                out.push(transform);
            }
            crate::ast::WriteCon::Delete { key, .. } => out.push(key),
        },
        EnumConstruct { args, .. } => out.extend(args.iter()),
        StrInterp(parts) => out.extend(parts.iter().filter_map(|p| match p {
            crate::ast::StrPart::Interp(x) => Some(x),
            _ => None,
        })),
        _ => {}
    }
    out
}

fn is_empty_set(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::SetLiteral(v) if v.is_empty())
}

/// `lambda [k](row) { row.<pk> == k }` (either comparison order).
fn is_key_predicate(predicate: &Expr, key_var: &str) -> bool {
    let ExprKind::Lambda(lam) = &predicate.kind else { return false };
    let Some(row) = lam.params.first().and_then(|p| match &p.pat.kind {
        PatternKind::Bind(id) => Some(id.node.as_str()),
        _ => None,
    }) else { return false };
    let ExprKind::BinOp { op: BinOpKind::Eq, lhs, rhs } = &lam.body.kind else { return false };
    let is_field_of_row = |e: &Expr| {
        matches!(&e.kind, ExprKind::Field { base, .. }
            if matches!(&base.kind, ExprKind::Var(v) if v.node == row))
    };
    let is_key = |e: &Expr| matches!(&e.kind, ExprKind::Var(v) if v.node == key_var);
    (is_field_of_row(lhs) && is_key(rhs)) || (is_field_of_row(rhs) && is_key(lhs))
}

fn is_true_predicate(predicate: &Expr) -> bool {
    matches!(&predicate.kind, ExprKind::Lambda(lam)
        if matches!(&lam.body.kind, ExprKind::Lit(Literal::Bool(true))))
}

fn has_primed(e: &Expr) -> bool {
    if matches!(&e.kind, ExprKind::Primed(_)) {
        return true;
    }
    child_exprs(e).iter().any(|c| has_primed(c))
}

fn ty_name(k: &TypeKind) -> &'static str {
    match k {
        TypeKind::Bool => "bool",
        TypeKind::Int => "int",
        TypeKind::Float => "float",
        TypeKind::Decimal(_) => "decimal",
        TypeKind::String => "string",
        TypeKind::Date => "date",
        _ => "structured type",
    }
}

/// Short display name of an expression kind, for diagnostics.
pub(crate) fn kind_name(e: &Expr) -> String {
    use ExprKind::*;
    match &e.kind {
        Lit(l) => format!("literal {l:?}"),
        Var(i) => format!("variable `{}`", i.node),
        Block { .. } => "block".into(),
        Let { .. } => "let".into(),
        Lambda(_) => "lambda".into(),
        App { .. } => "application".into(),
        Call(c) => format!("call `{}`", c.name.node),
        Match { .. } => "match".into(),
        If { .. } => "if".into(),
        Try(_) => "try `?`".into(),
        RecordLit { .. } => "record literal".into(),
        RecordUpd { .. } => "record update".into(),
        Tuple(_) => "tuple".into(),
        Vector(_) => "vector literal".into(),
        SetLiteral(_) => "set literal".into(),
        SetFilter { .. } => "set comprehension".into(),
        SetMap { .. } => "set comprehension".into(),
        BagLiteral(_) => "bag literal".into(),
        BagMap { .. } => "bag comprehension".into(),
        MapLit(_) => "map literal".into(),
        OptionSome(_) => "some(...)".into(),
        OptionNone => "none".into(),
        StrInterp(_) => "string interpolation".into(),
        Quantifier { kind, .. } => format!("quantifier `{kind:?}`"),
        Cast { .. } => "cast".into(),
        BinOp { op, .. } => format!("operator `{op:?}`"),
        UnOp { op, .. } => format!("operator `{op:?}`"),
        Field { name, .. } => format!("field access `.{n}`", n = name.node),
        TupleProj { .. } => "tuple projection".into(),
        MethodCall { name, .. } => format!("method call `.{n}()`", n = name.node),
        Primed(_) => "prime".into(),
        ReadPrim { table, .. } => format!("read `{}`", table.node),
        WriteCon(_) => "write constructor".into(),
        EnumConstruct { name, .. } => format!("enum constructor `{}`", name.node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::compile_project;
    use cql_mc::counterexample::Verdict;

    const BANK: &str = include_str!("../../../examples/bank_project/src/bank.cql");

    fn lower(src: &str, config: &McConfig) -> (Option<McSpec>, DiagBag) {
        let (out, bag) = compile_project(&[("test.cql".to_string(), src.to_string())]);
        assert!(bag.errors().is_empty(), "frontend errors:\n{}", bag.render());
        let out = out.expect("compiles");
        lower_to_mc(&out, &[("test.cql".to_string(), src.to_string())], config)
    }

    fn bank_config() -> McConfig {
        let mut config = McConfig::default();
        config
            .domains
            .insert("accounts.id".to_string(), DomainBound::Values(vec![1, 2]));
        config.domains.insert(
            "accounts.balance".to_string(),
            DomainBound::Values(vec![0, 6000, 4000]),
        );
        config
    }

    // ---- DomainBound parsing ----

    #[test]
    fn domain_bound_from_toml() {
        let range = DomainBound::from_toml("t.id", &toml::Value::String("1..3".into())).unwrap();
        assert_eq!(range, DomainBound::Values(vec![1, 2, 3]));
        let arr = DomainBound::from_toml(
            "t.v",
            &toml::Value::Array(vec![toml::Value::Integer(0), toml::Value::Integer(-2)]),
        )
        .unwrap();
        assert_eq!(arr, DomainBound::Values(vec![0, -2]));
        let rows = DomainBound::from_toml("t.rows", &toml::Value::Integer(3)).unwrap();
        assert_eq!(rows, DomainBound::Rows(3));
        assert!(DomainBound::from_toml("t.id", &toml::Value::String("nope".into())).is_err());
        assert!(DomainBound::from_toml("t.id", &toml::Value::String("3..1".into())).is_err());
        assert!(DomainBound::from_toml(
            "t.v",
            &toml::Value::Array(vec![toml::Value::Float(1.5)])
        )
        .is_err());
    }

    // ---- bank: the driving example ----

    #[test]
    fn bank_lowers_to_expected_spec() {
        let (spec, bag) = lower(BANK, &bank_config());
        assert!(bag.errors().is_empty(), "lowering errors:\n{}", bag.render());
        let spec = spec.expect("bank lowers");

        // Table: accounts (int key id, int value balance; owner ignored with warning).
        assert_eq!(spec.tables.len(), 1);
        assert_eq!(spec.tables[0].name, "accounts");
        assert!(
            bag.warnings().iter().any(|w| w.message().contains("owner")),
            "expected ignored-field warning, got {:?}",
            bag.warnings()
        );

        // Fixture initial state from the test block.
        assert_eq!(spec.init, vec![(0, 1, 6000), (0, 2, 4000)]);

        // One transition: transfer with 3 int params and inferred domains.
        assert_eq!(spec.transitions.len(), 1);
        let tr = &spec.transitions[0];
        assert_eq!(tr.name, "transfer");
        assert_eq!(tr.params, vec![ir::Ty::Int; 3]);
        assert_eq!(tr.param_domains[0], vec![1, 2]); // from_id: key of accounts
        assert_eq!(tr.param_domains[1], vec![1, 2]); // to_id
        assert_eq!(tr.param_domains[2], vec![0, 6000, 4000]); // amt: accounts.balance domain (as written)

        // Guard: both keys present ∧ from.balance >= amt.
        let ir::McExpr::And(gs) = &tr.guard else {
            panic!("guard should be a conjunction: {:?}", tr.guard);
        };
        assert_eq!(gs.len(), 3);
        assert!(matches!(&gs[0], ir::McExpr::Contains { table: 0, .. }));
        assert!(matches!(&gs[1], ir::McExpr::Contains { table: 0, .. }));
        assert!(matches!(&gs[2], ir::McExpr::Ge(..)));

        // Two updates: from -= amt, to += amt.
        assert_eq!(tr.updates.len(), 2);
        assert_eq!(tr.updates[0].kind, UpdateKind::Update);
        assert_eq!(tr.updates[1].kind, UpdateKind::Update);
        assert!(matches!(tr.updates[0].value, Some(ir::McExpr::Sub(..))));
        assert!(matches!(tr.updates[1].value, Some(ir::McExpr::Add(..))));

        // Properties: balance_conserved + no_negative lowered; transfer_preserves
        // (prime) skipped with a warning.
        assert_eq!(spec.properties.len(), 2);
        let names: Vec<&str> = spec.properties.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["balance_conserved", "no_negative"]);
        assert!(
            bag.warnings().iter().any(|w| w.message().contains("transfer_preserves")),
            "expected prime-skip warning"
        );

        // balance_conserved = [](sum(accounts) = 10000).
        match &spec.properties[0].kind {
            PropertyKind::Always(ir::McExpr::Eq(l, r)) => {
                assert!(matches!(&**l, ir::McExpr::Sum { table: 0, domain } if *domain == vec![1, 2]));
                assert!(matches!(&**r, ir::McExpr::IntLit(10000)));
            }
            other => panic!("unexpected property shape: {other:?}"),
        }
        // no_negative = ⋀ k∈{1,2}. contains(k) ⇒ select(k) >= 0.
        match &spec.properties[1].kind {
            PropertyKind::Always(ir::McExpr::And(cs)) => {
                assert_eq!(cs.len(), 2);
                assert!(matches!(&cs[0], ir::McExpr::Implies(..)));
            }
            other => panic!("unexpected property shape: {other:?}"),
        }

        assert_eq!(spec.depth, 8); // trace length default
    }

    #[test]
    fn bank_spec_is_proved_by_stateright() {
        let (spec, bag) = lower(BANK, &bank_config());
        assert!(bag.errors().is_empty(), "{}", bag.render());
        let spec = spec.unwrap();
        let verdicts = cql_mc::stateright_be::check(&spec);
        assert_eq!(verdicts.len(), 2);
        for v in &verdicts {
            assert!(
                matches!(v, Verdict::Proved { .. }),
                "expected all proved, got {v}"
            );
        }
    }

    #[test]
    fn buggy_transfer_yields_counterexample() {
        // Conservation bug: the credit leg debits too (both legs `- amt`).
        // State space stays finite (guards bound balances below).
        let buggy = BANK.replace(
            "v.balance + amt } }) }",
            "v.balance - amt } }) }",
        );
        assert_ne!(buggy, BANK);
        let (spec, bag) = lower(&buggy, &bank_config());
        assert!(bag.errors().is_empty(), "{}", bag.render());
        let spec = spec.unwrap();
        let verdicts = cql_mc::stateright_be::check(&spec);
        let cex = verdicts.iter().find_map(|v| match v {
            Verdict::Counterexample { property, cex } if property == "balance_conserved" => {
                Some(cex)
            }
            _ => None,
        });
        let cex = cex.expect("counterexample for balance_conserved");
        assert!(cex.steps.len() >= 2, "trace should have init + violating step");
        let rendered = cex.render(&spec);
        assert!(rendered.contains("transfer("));
        assert!(rendered.contains("accounts {"));
    }

    // ---- unsupported-construct diagnostics ----

    #[test]
    fn string_keyed_table_rejected() {
        let src = r#"
module m;
table users { name: string, age: int } primary key {name}
action noop() -> set<write_op> == { set {} }
"#;
        let (_spec, bag) = lower(src, &McConfig::default());
        assert!(bag.has_errors());
        assert!(bag.render().contains("not checkable"));
        assert!(bag.render().contains("primary key"));
    }

    #[test]
    fn multi_int_value_fields_rejected() {
        let src = r#"
module m;
table t { id: int, a: int, b: int } primary key {id}
"#;
        let (_spec, bag) = lower(src, &McConfig::default());
        assert!(bag.has_errors());
        assert!(bag.render().contains("exactly one non-key `int` field"));
    }

    #[test]
    fn division_rejected() {
        let src = r#"
module m;
table t { id: int, v: int } primary key {id}
query half() -> int == { fold(to_vector(t), 0, lambda(acc, x) { acc + x.v }) / 2 }
property p == [](half() >= 0)
"#;
        let (_spec, bag) = lower(src, &McConfig::default());
        assert!(bag.has_errors());
        assert!(bag.render().contains("Div"));
    }

    #[test]
    fn recursion_rejected_at_depth_bound() {
        let src = r#"
module m;
table t { id: int, v: int } primary key {id}
function f(n: int) -> int == { f(n - 1) }
property p == [](f(3) >= 0)
"#;
        let mut config = McConfig::default();
        config.depth_default = 4;
        let (_spec, bag) = lower(src, &config);
        assert!(bag.has_errors());
        assert!(bag.render().contains("recursive operator `f`"), "{}", bag.render());
    }

    #[test]
    fn unsupported_property_kinds_skipped_with_warning() {
        let src = r#"
module m;
table t { id: int, v: int } primary key {id}
property ok == [](\A x \in t : x.v >= 0)
property ltl == (\E x \in t : x.v = 1) until (\A x \in t : x.v = 2)
property ev == <>(\E x \in t : x.v = 1)
"#;
        let mut config = McConfig::default();
        config.domains.insert("t.id".to_string(), DomainBound::Values(vec![1]));
        let (spec, bag) = lower(src, &config);
        assert!(bag.errors().is_empty(), "{}", bag.render());
        let spec = spec.unwrap();
        // `ok` (Always) and `ev` (Eventually) lowered; `ltl` skipped.
        assert_eq!(spec.properties.len(), 2);
        assert!(matches!(spec.properties[0].kind, PropertyKind::Always(_)));
        assert!(matches!(spec.properties[1].kind, PropertyKind::Eventually(_)));
        assert!(bag.warnings().iter().any(|w| w.message().contains("ltl")));
    }

    #[test]
    fn insert_delete_writes_lower() {
        let src = r#"
module m;
table t { id: int, v: int } primary key {id}
action add(k: int, val: int) -> set<write_op> == {
    set { insert(t, record { id: k, v: val }) }
}
action del(k: int) -> set<write_op> == {
    set { delete(t, k) }
}
"#;
        let mut config = McConfig::default();
        config.domains.insert("t.id".to_string(), DomainBound::Values(vec![1, 2]));
        config.domains.insert("t.v".to_string(), DomainBound::Values(vec![0, 1]));
        let (spec, bag) = lower(src, &config);
        assert!(bag.errors().is_empty(), "{}", bag.render());
        let spec = spec.unwrap();
        assert_eq!(spec.transitions.len(), 2);
        assert_eq!(spec.transitions[0].updates[0].kind, UpdateKind::Insert);
        assert_eq!(spec.transitions[1].updates[0].kind, UpdateKind::Delete);
        assert_eq!(spec.transitions[0].param_domains[0], vec![1, 2]);
        assert_eq!(spec.transitions[0].param_domains[1], vec![0, 1]);
    }

    #[test]
    fn multi_module_project_rejected() {
        let sources = [
            (
                "a.cql".to_string(),
                "module a;\ntable ta { id: int, v: int } primary key {id}".to_string(),
            ),
            (
                "b.cql".to_string(),
                "module b;\ntable tb { id: int, v: int } primary key {id}".to_string(),
            ),
        ];
        let (out, bag) = compile_project(&sources);
        assert!(bag.errors().is_empty(), "{}", bag.render());
        let (_spec, bag) = lower_to_mc(&out.unwrap(), &sources, &McConfig::default());
        assert!(bag.has_errors());
        assert!(bag.render().contains("single-module"));
    }
}
