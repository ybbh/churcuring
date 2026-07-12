//! Termination pass (doc/cql.md §3.4, §5.4; pipeline §D.3).
//!
//! Classifies every operator into the **theorem layer** (provably
//! terminating) or the **bounded layer** (general recursion, verified up to a
//! depth bound by model checking):
//!
//! - Non-recursive operators → theorem layer.
//! - `recursive` operators are structurally checked: one recursive parameter
//!   (the first inductive-typed parameter, or `decreases <param>`), every
//!   self-call passing a **strict subterm** (pattern bindings / projection
//!   chains; `let` aliases do not propagate subterm-ness), and no mutual
//!   recursion (call-graph SCC > 1 containing a `recursive` operator ⇒
//!   error). Passing operators are theorem-layer.
//! - General recursion (unmarked self/mutual recursion) is not an error: the
//!   operator is bounded-layer with its `with depth n` annotation (if any).
//!   If it in fact satisfies the structural rules, a hint warning suggests
//!   upgrading to `recursive`.

use std::collections::{HashMap, HashSet};

use miette::NamedSource;

use crate::ast::*;
use crate::diag::{CqlError, DiagBag};
use crate::resolve::{Callee, ResolvedModule};

/// Termination classification of one operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermClass {
    /// Provably terminating: non-recursive, or `recursive` and structurally
    /// verified.
    Theorem,
    /// General recursion: native execution (stack-exhaustion trap), model
    /// checked up to `depth` (falls back to project verification config).
    Bounded { depth: Option<u64> },
}

/// Per-operator termination classification.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminationInfo {
    pub classes: HashMap<String, TermClass>,
}

const REWRITE_HELP: &str =
    "rewrite using cons-style structural recursion or `fold`, or drop `recursive` to use general recursion (§3.4)";

/// Run the termination pass on a resolved module.
pub fn check_termination(resolved: &ResolvedModule) -> Result<TerminationInfo, DiagBag> {
    let src = NamedSource::new(format!("{}.cql", resolved.module.name.node), String::new());
    check_termination_with_src(resolved, src)
}

/// Like [`check_termination`] but attaches `src` to diagnostics.
pub fn check_termination_with_src(
    resolved: &ResolvedModule,
    src: NamedSource<String>,
) -> Result<TerminationInfo, DiagBag> {
    let mut t = Terminator::new(resolved, src);
    t.run();
    t.diags.into_result(t.info)
}

struct Terminator<'a> {
    resolved: &'a ResolvedModule,
    diags: DiagBag,
    src: NamedSource<String>,
    info: TerminationInfo,
    /// Module-local operators by name.
    ops: HashMap<String, &'a OperatorDecl>,
    /// Call edges between module-local operators (caller → callees).
    edges: HashMap<String, HashSet<String>>,
}

impl<'a> Terminator<'a> {
    fn new(resolved: &'a ResolvedModule, src: NamedSource<String>) -> Self {
        let mut ops = HashMap::new();
        for item in &resolved.module.items {
            if let Item::Operator(o) = item {
                ops.insert(o.name.node.clone(), o);
            }
        }
        // Build the module-local call graph from resolution side tables: we
        // re-walk bodies to attribute each call to its enclosing operator.
        let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
        for item in &resolved.module.items {
            if let Item::Operator(o) = item {
                let mut callees = HashSet::new();
                if let Some(body) = &o.body {
                    collect_calls(resolved, body, &ops, &mut callees);
                }
                edges.insert(o.name.node.clone(), callees);
            }
        }
        Terminator { resolved, diags: DiagBag::new(), src, info: TerminationInfo::default(), ops, edges }
    }

    fn err(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_error(CqlError::new(self.src.clone(), span, message, help));
    }

    fn warn(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_warning(CqlError::new(self.src.clone(), span, message, help));
    }

    fn run(&mut self) {
        // Validate `decreases` annotations on every `recursive` operator,
        // even those without self-recursion.
        let bad_decreases: Vec<(Span, String, String)> = self
            .ops
            .values()
            .filter_map(|op| {
                let d = op.decreases.as_ref()?;
                if op.recursive && !op.params.iter().any(|p| p.name.node == d.node) {
                    Some((d.span, d.node.clone(), op.name.node.clone()))
                } else {
                    None
                }
            })
            .collect();
        for (span, d, op_name) in bad_decreases {
            self.err(
                span,
                format!("`decreases` names `{}`, which is not a parameter of `{}`", d, op_name),
                None,
            );
        }
        let sccs = tarjan_scc(&self.edges);
        for scc in sccs {
            let recursive_members: Vec<&str> = scc
                .iter()
                .filter(|n| self.ops.get(*n).is_some_and(|o| o.recursive))
                .map(|s| s.as_str())
                .collect();
            if scc.len() > 1 {
                // Mutual recursion: only an error for `recursive` operators.
                for name in &recursive_members {
                    let op = self.ops[*name];
                    self.err(
                        op.name.span,
                        format!("`recursive` operator `{}` is mutually recursive with {}", name, scc.iter().filter(|n| *n != name).cloned().collect::<Vec<_>>().join(", ")),
                        Some("structural recursion must be self-recursive only; split the cycle or use general recursion".to_string()),
                    );
                    self.info.classes.insert(name.to_string(), TermClass::Bounded { depth: op.depth });
                }
                for name in &scc {
                    if !recursive_members.contains(&name.as_str()) {
                        let op = self.ops[name];
                        self.info.classes.insert(name.clone(), TermClass::Bounded { depth: op.depth });
                    }
                }
                continue;
            }
            let name = scc[0].clone();
            let op = self.ops[&name];
            let self_recursive = self.edges[&name].contains(&name);
            if !self_recursive {
                // No recursion at all: theorem layer.
                self.info.classes.insert(name, TermClass::Theorem);
                continue;
            }
            if op.recursive {
                let mut sc = StructuralCheck::new(self, op, true);
                if sc.check() {
                    self.info.classes.insert(name, TermClass::Theorem);
                } else {
                    // Errors already reported; classify as bounded so later
                    // passes have an entry.
                    self.info.classes.insert(name, TermClass::Bounded { depth: op.depth });
                }
            } else {
                // General recursion: bounded layer. Hint if it would in fact
                // pass the structural check.
                let mut sc = StructuralCheck::new(self, op, false);
                if sc.check() {
                    self.warn(
                        op.name.span,
                        format!("operator `{}` is structurally recursive and could be marked `recursive`", name),
                        Some("adding `recursive` upgrades it to the theorem layer (§3.4)".to_string()),
                    );
                }
                self.info.classes.insert(name, TermClass::Bounded { depth: op.depth });
            }
        }
    }

    /// Is this type inductive (§3.4): enum, vector, tuple, or record literal?
    fn is_inductive(&self, ty: &Type) -> bool {
        match &ty.kind {
            TypeKind::Vector(_) | TypeKind::Tuple(_) | TypeKind::Record(_) => true,
            TypeKind::Named { name, .. } => self.resolved.resolved.known_enums.contains(&name.node),
            _ => false,
        }
    }
}

/// Collect module-local operator calls inside an expression.
fn collect_calls(
    resolved: &ResolvedModule,
    e: &Expr,
    ops: &HashMap<String, &OperatorDecl>,
    out: &mut HashSet<String>,
) {
    if let ExprKind::Call(call) = &e.kind {
        if let Some(Callee::Operator { name, module_local: true, .. }) =
            resolved.resolved.callee.get(&call.name.span)
        {
            if ops.contains_key(name) {
                out.insert(name.clone());
            }
        }
    }
    walk_children(e, &mut |child| collect_calls(resolved, child, ops, out));
}

/// Iterative Tarjan SCC over the operator call graph.
fn tarjan_scc(edges: &HashMap<String, HashSet<String>>) -> Vec<Vec<String>> {
    let mut index_counter = 0usize;
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut lowlink: HashMap<&str, usize> = HashMap::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut sccs: Vec<Vec<String>> = Vec::new();

    let mut names: Vec<&str> = edges.keys().map(|s| s.as_str()).collect();
    names.sort();

    for start in names {
        if index.contains_key(start) {
            continue;
        }
        // Explicit DFS stack: (node, sorted-successor index).
        let mut work: Vec<(&str, usize)> = vec![(start, 0)];
        index.insert(start, index_counter);
        lowlink.insert(start, index_counter);
        index_counter += 1;
        stack.push(start);
        on_stack.insert(start);

        while let Some((node, succ_i)) = work.last_mut() {
            let succs: Vec<&str> = {
                let mut v: Vec<&str> = edges
                    .get(*node)
                    .map(|s| s.iter().map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                v.sort();
                v
            };
            if *succ_i < succs.len() {
                let succ = succs[*succ_i];
                *succ_i += 1;
                if !index.contains_key(succ) {
                    index.insert(succ, index_counter);
                    lowlink.insert(succ, index_counter);
                    index_counter += 1;
                    stack.push(succ);
                    on_stack.insert(succ);
                    work.push((succ, 0));
                } else if on_stack.contains(succ) {
                    let l = lowlink[*node].min(index[succ]);
                    lowlink.insert(node, l);
                }
            } else {
                let node = *node;
                work.pop();
                if let Some((parent, _)) = work.last() {
                    let l = lowlink[parent].min(lowlink[node]);
                    lowlink.insert(parent, l);
                }
                if lowlink[node] == index[node] {
                    let mut scc = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack.remove(w);
                        scc.push(w.to_string());
                        if w == node {
                            break;
                        }
                    }
                    scc.sort();
                    sccs.push(scc);
                }
            }
        }
    }
    sccs
}

/// Structural-recursion check for one operator (§3.4).
struct StructuralCheck<'t, 'a> {
    t: &'t mut Terminator<'a>,
    op: &'a OperatorDecl,
    /// Whether to report errors (false for the hint-only check on general
    /// recursion).
    report: bool,
    failed: bool,
}

impl StructuralCheck<'_, '_> {
    fn new<'t, 'a>(t: &'t mut Terminator<'a>, op: &'a OperatorDecl, report: bool) -> StructuralCheck<'t, 'a> {
        StructuralCheck { t, op, report, failed: false }
    }

    /// Determine the recursive parameter and check every self-call. Returns
    /// true if the operator satisfies the structural-recursion rules.
    fn check(&mut self) -> bool {
        let Some(rec_param) = self.recursive_param() else {
            return false;
        };
        let rec_index = self.op.params.iter().position(|p| p.name.node == rec_param).unwrap();
        if let Some(body) = &self.op.body {
            let subterms = HashSet::new();
            self.expr(body, &rec_param, rec_index, &subterms);
        }
        !self.failed
    }

    /// The recursive parameter: `decreases <param>` if given, else the first
    /// parameter of inductive type.
    fn recursive_param(&mut self) -> Option<String> {
        if let Some(d) = &self.op.decreases {
            match self.op.params.iter().find(|p| p.name.node == d.node) {
                None => {
                    self.fail(
                        d.span,
                        format!("`decreases` names `{}`, which is not a parameter of `{}`", d.node, self.op.name.node),
                        None,
                    );
                    return None;
                }
                Some(p) => {
                    if !self.t.is_inductive(&p.ty) {
                        self.fail(
                            p.ty.span,
                            format!("recursive parameter `{}` must have an inductive type (enum, vector, tuple or record)", p.name.node),
                            Some(REWRITE_HELP.to_string()),
                        );
                        return None;
                    }
                    return Some(p.name.node.clone());
                }
            }
        }
        match self.op.params.iter().find(|p| self.t.is_inductive(&p.ty)) {
            Some(p) => Some(p.name.node.clone()),
            None => {
                self.fail(
                    self.op.name.span,
                    format!("`recursive` operator `{}` has no parameter of inductive type", self.op.name.node),
                    Some(REWRITE_HELP.to_string()),
                );
                None
            }
        }
    }

    fn fail(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.failed = true;
        if self.report {
            self.t.err(span, message, help);
        }
    }

    /// Is `e` a strict subterm expression: a variable in `subterms`, or a
    /// pure projection chain (`.field`/`.0`) rooted at one?
    fn is_subterm_expr(e: &Expr, subterms: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::Var(v) => subterms.contains(&v.node),
            ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => {
                Self::is_subterm_expr(base, subterms)
            }
            _ => false,
        }
    }

    /// Is `e` the recursive parameter itself, a known subterm, or a
    /// projection chain rooted at either? (Matching on such an expression
    /// yields subterm bindings.)
    fn is_destructure_root(e: &Expr, rec_param: &str, subterms: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::Var(v) => v.node == rec_param || subterms.contains(&v.node),
            ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => {
                Self::is_destructure_root(base, rec_param, subterms)
            }
            _ => false,
        }
    }

    /// Walk an expression checking self-calls; `subterms` holds the variables
    /// known to be strict subterms of the recursive parameter in this scope.
    fn expr(&mut self, e: &Expr, rec_param: &str, rec_index: usize, subterms: &HashSet<String>) {
        // Self-call: the argument at the recursive parameter's position must
        // be a strict subterm expression.
        if let ExprKind::Call(call) = &e.kind {
            if matches!(
                self.t.resolved.resolved.callee.get(&call.name.span),
                Some(Callee::Operator { name, module_local: true, .. }) if *name == self.op.name.node
            ) {
                match self.arg_for_param(call, rec_param, rec_index) {
                    Some(arg) if Self::is_subterm_expr(arg, subterms) => {}
                    Some(arg) => self.fail(
                        arg.span,
                        format!("recursive call to `{}` does not decrease: argument for `{}` is not a strict subterm", self.op.name.node, rec_param),
                        Some(REWRITE_HELP.to_string()),
                    ),
                    None => self.fail(
                        call.name.span,
                        format!("recursive call to `{}` is missing an argument for `{}`", self.op.name.node, rec_param),
                        None,
                    ),
                }
            }
        }

        match &e.kind {
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, rec_param, rec_index, subterms);
                for arm in arms {
                    // Pattern bindings over a subterm scrutinee are themselves
                    // strict subterms (variant payloads, tuple components,
                    // record fields, cons head/tail).
                    let mut arm_subterms = subterms.clone();
                    if Self::is_destructure_root(scrutinee, rec_param, subterms) {
                        for b in arm.pat.bound_idents() {
                            arm_subterms.insert(b.node.clone());
                        }
                    }
                    self.expr(&arm.body, rec_param, rec_index, &arm_subterms);
                }
            }
            ExprKind::Lambda(l) => {
                // Recursive calls inside pure lambdas are checked at the call
                // site with the lexically visible subterm set (§3.4).
                self.expr(&l.body, rec_param, rec_index, subterms);
            }
            _ => {
                // Note: `let` bindings deliberately do NOT extend `subterms`
                // — subterm-ness does not flow through aliases (§3.4).
                walk_children(e, &mut |child| self.expr(child, rec_param, rec_index, subterms));
            }
        }
    }

    /// Find the argument passed for parameter `param` (by position or by
    /// name), mirroring the named-argument rules checked by resolve.
    fn arg_for_param<'c>(&self, call: &'c Call, param: &str, index: usize) -> Option<&'c Expr> {
        // Named arguments win if present.
        for a in &call.args {
            if let Some(n) = &a.name {
                if n.node == param {
                    return Some(&a.value);
                }
            }
        }
        // Otherwise the positional argument at the parameter's index —
        // provided no named argument sits before it (resolve rejects
        // positional-after-named, so position = index for the prefix).
        let positional: Vec<&Arg> = call.args.iter().filter(|a| a.name.is_none()).collect();
        if index < positional.len() {
            Some(&positional[index].value)
        } else {
            None
        }
    }
}

/// Recurse into all child expressions of `e` (non-scoping traversal helper
/// shared by the passes' auxiliary analyses).
pub(crate) fn walk_children(e: &Expr, f: &mut impl FnMut(&Expr)) {
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Var(_) | ExprKind::OptionNone => {}
        ExprKind::Block { lets, tail } => {
            for l in lets {
                f(&l.value);
            }
            f(tail);
        }
        ExprKind::Let { value, body, .. } => {
            f(value);
            f(body);
        }
        ExprKind::Lambda(l) => f(&l.body),
        ExprKind::App { func, args } => {
            f(func);
            for a in args {
                f(&a.value);
            }
        }
        ExprKind::Call(c) => {
            for a in &c.args {
                f(&a.value);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                f(&arm.body);
            }
        }
        ExprKind::If { cond, then_br, else_br } => {
            f(cond);
            f(then_br);
            f(else_br);
        }
        ExprKind::Try(inner) => f(inner),
        ExprKind::RecordLit { fields } => {
            for fi in fields {
                f(&fi.value);
            }
        }
        ExprKind::RecordUpd { base, fields } => {
            f(base);
            for fi in fields {
                f(&fi.value);
            }
        }
        ExprKind::Tuple(items) | ExprKind::Vector(items) | ExprKind::SetLiteral(items)
        | ExprKind::BagLiteral(items) => {
            for item in items {
                f(item);
            }
        }
        ExprKind::SetFilter { source, pred, .. } => {
            f(source);
            f(pred);
        }
        ExprKind::SetMap { elem, gens } | ExprKind::BagMap { elem, gens } => {
            f(elem);
            for g in gens {
                f(&g.source);
            }
        }
        ExprKind::MapLit(entries) => {
            for (k, v) in entries {
                f(k);
                f(v);
            }
        }
        ExprKind::OptionSome(inner) => f(inner),
        ExprKind::StrInterp(parts) => {
            for p in parts {
                if let StrPart::Interp(inner) = p {
                    f(inner);
                }
            }
        }
        ExprKind::Quantifier { gens, body, .. } => {
            for g in gens {
                f(&g.source);
            }
            f(body);
        }
        ExprKind::Cast { expr, .. } => f(expr),
        ExprKind::BinOp { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        ExprKind::UnOp { operand, .. } => f(operand),
        ExprKind::Primed(inner) => f(inner),
        ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => f(base),
        ExprKind::MethodCall { recv, args, .. } => {
            f(recv);
            for a in args {
                f(&a.value);
            }
        }
        ExprKind::ReadPrim { predicate, .. } => f(predicate),
        ExprKind::WriteCon(w) => match w {
            WriteCon::Insert { row, .. } => f(row),
            WriteCon::Update { key, transform, .. } => {
                f(key);
                f(transform);
            }
            WriteCon::Delete { key, .. } => f(key),
        },
        ExprKind::EnumConstruct { args, .. } => {
            for a in args {
                f(a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::{decl, expr, pat, ty};
    use crate::resolve::resolve_module;

    fn run(items: Vec<Item>) -> (Result<TerminationInfo, DiagBag>, DiagBag) {
        let r = resolve_module(decl::module("test", items), &[]).expect("resolve failed");
        let src = NamedSource::new("test.cql", String::new());
        let mut t = Terminator::new(&r, src);
        t.run();
        let diags = t.diags.clone();
        (t.diags.into_result(t.info), diags)
    }

    fn msgs(bag: &DiagBag) -> Vec<String> {
        bag.errors().iter().map(|e| e.message().to_string()).collect()
    }

    fn tree_enum() -> Item {
        decl::enum_(
            "tree",
            vec![
                decl::variant_tuple("leaf", vec![ty::int()]),
                decl::variant_tuple("node", vec![ty::named("tree"), ty::int(), ty::named("tree")]),
            ],
        )
    }

    /// §8.6 inorder: structural recursion over `tree`.
    fn inorder_items() -> Vec<Item> {
        let body = expr::match_(
            expr::var("t"),
            vec![
                (pat::variant("leaf", vec![pat::bind("v")]), expr::vector(vec![expr::var("v")])),
                (
                    pat::variant("node", vec![pat::bind("l"), pat::bind("x"), pat::bind("r")]),
                    expr::call(
                        "concat_vector",
                        vec![
                            expr::call(
                                "concat_vector",
                                vec![expr::call("inorder", vec![expr::var("l")]), expr::vector(vec![expr::var("x")])],
                            ),
                            expr::call("inorder", vec![expr::var("r")]),
                        ],
                    ),
                ),
            ],
        );
        vec![
            tree_enum(),
            decl::function_rec(
                "inorder",
                vec![decl::param("t", ty::named("tree"))],
                ty::vector(ty::int()),
                body,
            ),
        ]
    }

    #[test]
    fn inorder_structural_recursion_passes() {
        let (res, diags) = run(inorder_items());
        assert!(diags.warnings().is_empty(), "unexpected warnings: {:?}", diags.warnings().iter().map(|w| w.message()).collect::<Vec<_>>());
        let info = res.expect("inorder passes the structural check");
        assert_eq!(info.classes.get("inorder"), Some(&TermClass::Theorem));
    }

    #[test]
    fn non_recursive_is_theorem() {
        let items = vec![decl::function(
            "f",
            vec![decl::param("x", ty::int())],
            ty::int(),
            expr::binop(BinOpKind::Add, expr::var("x"), expr::int(1)),
        )];
        let (res, _) = run(items);
        let info = res.expect("ok");
        assert_eq!(info.classes.get("f"), Some(&TermClass::Theorem));
    }

    #[test]
    fn mutual_recursion_with_recursive_errors() {
        // a (recursive) calls b, b calls a.
        let items = vec![
            decl::function_rec(
                "a",
                vec![decl::param("t", ty::named("tree"))],
                ty::int(),
                expr::call("b", vec![expr::var("t")]),
            ),
            decl::function(
                "b",
                vec![decl::param("t", ty::named("tree"))],
                ty::int(),
                expr::call("a", vec![expr::var("t")]),
            ),
            tree_enum(),
        ];
        let (res, _) = run(items);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("mutually recursive")));
    }

    #[test]
    fn let_alias_bypassing_subterm_errors() {
        // recursive f: matches tail into `rest`, then aliases via let and
        // recurses on the alias — subterm-ness does not flow through let.
        let body = expr::match_(
            expr::var("xs"),
            vec![
                (pat::cons_nil(), expr::int(0)),
                (
                    pat::cons(pat::bind("h"), pat::bind("rest")),
                    expr::block(
                        vec![expr::let_(pat::bind("r2"), expr::var("rest"))],
                        expr::call("f", vec![expr::var("r2")]),
                    ),
                ),
            ],
        );
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            body,
        )];
        let (res, _) = run(items);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not a strict subterm")));

        // Direct recursion on `rest` is fine.
        let ok_body = expr::match_(
            expr::var("xs"),
            vec![
                (pat::cons_nil(), expr::int(0)),
                (pat::cons(pat::bind("h"), pat::bind("rest")), expr::call("f", vec![expr::var("rest")])),
            ],
        );
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            ok_body,
        )];
        let (res, _) = run(items);
        assert!(res.is_ok(), "direct cons-tail recursion should pass");
    }

    #[test]
    fn projection_chain_subterm_ok() {
        // Recurse on t.0 where t is a tuple parameter.
        let body = expr::match_(
            expr::tuple_proj(expr::var("t"), 0),
            vec![
                (pat::cons_nil(), expr::int(0)),
                (
                    pat::cons(pat::bind("h"), pat::bind("rest")),
                    expr::call("f", vec![expr::tuple(vec![expr::var("rest"), expr::int(0)])]),
                ),
            ],
        );
        // Recursive param is the tuple `t`; the call passes a tuple whose first
        // component is `rest`... that's not a projection of t, so use a
        // simpler form: recurse on the projection of a subterm var.
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param("t", ty::tuple(vec![ty::vector(ty::int()), ty::int()]))],
            ty::int(),
            body,
        )];
        let (res, _) = run(items);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not a strict subterm")));

        // Proper projection-chain form: match p with a tuple pattern binding
        // `inner` (a subterm), then recurse on the projection `inner.0`.
        let body = expr::match_(
            expr::var("p"),
            vec![(
                pat::tuple(vec![pat::bind("inner"), pat::bind("n")]),
                expr::call("f", vec![expr::tuple_proj(expr::var("inner"), 0)]),
            )],
        );
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param(
                "p",
                ty::tuple(vec![ty::tuple(vec![ty::vector(ty::int()), ty::int()]), ty::int()]),
            )],
            ty::int(),
            body,
        )];
        let (res, diags) = run(items);
        assert!(res.is_ok(), "projection-of-subterm recursion should pass: {diags:?}");
    }

    #[test]
    fn gcd_general_recursion_is_bounded() {
        // gcd(a, b) = if b = 0 then a else gcd(b, a % b)
        let body = expr::if_(
            expr::binop(BinOpKind::Eq, expr::var("b"), expr::int(0)),
            expr::var("a"),
            expr::call("gcd", vec![expr::var("b"), expr::binop(BinOpKind::Mod, expr::var("a"), expr::var("b"))]),
        );
        let mut op = match decl::function(
            "gcd",
            vec![decl::param("a", ty::int()), decl::param("b", ty::int())],
            ty::int(),
            body,
        ) {
            Item::Operator(o) => o,
            _ => unreachable!(),
        };
        op.depth = Some(64);
        let (res, diags) = run(vec![Item::Operator(op)]);
        assert!(diags.warnings().is_empty(), "int params are not inductive: no hint expected");
        let info = res.expect("general recursion is not an error");
        assert_eq!(info.classes.get("gcd"), Some(&TermClass::Bounded { depth: Some(64) }));
    }

    #[test]
    fn structural_general_recursion_gets_hint() {
        // Same shape as inorder but without `recursive` — should be Bounded
        // with a hint warning.
        let items = inorder_items();
        let items: Vec<Item> = items
            .into_iter()
            .map(|it| match it {
                Item::Operator(mut o) => {
                    o.recursive = false;
                    Item::Operator(o)
                }
                other => other,
            })
            .collect();
        let (res, diags) = run(items);
        let info = res.expect("ok");
        assert_eq!(info.classes.get("inorder"), Some(&TermClass::Bounded { depth: None }));
        assert!(diags
            .warnings()
            .iter()
            .any(|w| w.message().contains("could be marked `recursive`")));
    }

    #[test]
    fn recursive_without_inductive_param_errors() {
        let items = vec![decl::function_rec(
            "count",
            vec![decl::param("n", ty::int())],
            ty::int(),
            expr::if_(
                expr::binop(BinOpKind::Eq, expr::var("n"), expr::int(0)),
                expr::int(0),
                expr::call("count", vec![expr::binop(BinOpKind::Sub, expr::var("n"), expr::int(1))]),
            ),
        )];
        let (res, _) = run(items);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("no parameter of inductive type")));
    }

    #[test]
    fn decreases_selects_param_and_bad_call_errors() {
        // recursive on the second param explicitly; passing the param itself
        // (not a strict subterm) must fail.
        let mut op = match decl::function_rec(
            "f",
            vec![decl::param("n", ty::int()), decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            expr::call("f", vec![expr::var("n"), expr::var("xs")]),
        ) {
            Item::Operator(o) => o,
            _ => unreachable!(),
        };
        op.decreases = Some(crate::ast::builder::id("xs"));
        let (res, _) = run(vec![Item::Operator(op)]);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not a strict subterm")));

        // decreases naming a non-parameter errors.
        let mut op = match decl::function_rec(
            "g",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            expr::int(0),
        ) {
            Item::Operator(o) => o,
            _ => unreachable!(),
        };
        op.decreases = Some(crate::ast::builder::id("nope"));
        let (res, _) = run(vec![Item::Operator(op)]);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("is not a parameter")));
    }

    #[test]
    fn recursive_call_inside_lambda_checked() {
        // Recursive call on a non-subterm hidden inside a pure lambda.
        let body = expr::match_(
            expr::var("xs"),
            vec![
                (pat::cons_nil(), expr::int(0)),
                (
                    pat::cons(pat::bind("h"), pat::bind("rest")),
                    expr::call(
                        "fold",
                        vec![
                            expr::var("rest"),
                            expr::int(0),
                            expr::lambda(&["xs"], vec![pat::bind("acc"), pat::bind("x")], expr::call("f", vec![expr::var("xs")])),
                        ],
                    ),
                ),
            ],
        );
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            body,
        )];
        // The lambda captures `xs` (the recursive param itself, not a
        // subterm) — the self-call inside must be rejected.
        let (res, _) = run(items);
        let bag = res.unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not a strict subterm")));

        // Recursing on `rest` inside a lambda is fine.
        let body = expr::match_(
            expr::var("xs"),
            vec![
                (pat::cons_nil(), expr::int(0)),
                (
                    pat::cons(pat::bind("h"), pat::bind("rest")),
                    expr::call(
                        "fold",
                        vec![
                            expr::var("rest"),
                            expr::int(0),
                            expr::lambda(&["rest"], vec![pat::bind("acc"), pat::bind("x")], expr::call("f", vec![expr::var("rest")])),
                        ],
                    ),
                ),
            ],
        );
        let items = vec![decl::function_rec(
            "f",
            vec![decl::param("xs", ty::vector(ty::int()))],
            ty::int(),
            body,
        )];
        let (res, _) = run(items);
        assert!(res.is_ok(), "subterm recursive call inside lambda should pass");
    }
}
