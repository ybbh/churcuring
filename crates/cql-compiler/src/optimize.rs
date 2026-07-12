//! Core-level optimization (doc/cql.md §D.3, read planning §5.5).
//!
//! Currently a pure analysis pass: every `read` primitive in the desugared
//! module is classified into a [`ReadPlan`] side table keyed by the read
//! expression's span. The desugared AST is not rewritten; code generation
//! consults `plans` to pick the access method for each read.
//!
//! Classification:
//!
//! - decompose the predicate lambda body into top-level `/\` conjuncts
//!   (unwrapping the `let` chains produced by block desugaring);
//! - a conjunct `u.c = e` (or `e = u.c`) is a *usable equality* on column `c`
//!   when `u` is the row parameter and `e` does not reference `u`;
//! - all primary-key columns covered ⇒ [`ReadPlan::PointLookup`];
//! - otherwise the first index (declaration order) on the table whose columns
//!   are all covered ⇒ [`ReadPlan::IndexScan`];
//! - otherwise [`ReadPlan::FullScan`].

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ast::*;
use crate::desugar::DesugaredModule;

/// The access plan chosen for one `read` primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadPlan {
    /// All primary-key columns are constrained by usable equalities.
    PointLookup,
    /// A declared secondary index covers the constrained columns.
    IndexScan { index: Ident },
    /// No usable index; scan the whole table.
    FullScan,
}

/// The output of the optimize pass: the desugared module plus read plans.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizedModule {
    pub desugared: DesugaredModule,
    /// Read plan per `read` primitive, keyed by the read expression's span.
    pub plans: HashMap<Span, ReadPlan>,
}

/// Classify every `read` primitive of a desugared module.
pub fn optimize_module(desugared: DesugaredModule) -> OptimizedModule {
    let module = &desugared.typed.resolved.module;
    // table name → (primary key, indexes in declaration order)
    let mut tables: HashMap<String, (Vec<String>, Vec<(Ident, Vec<String>)>)> = HashMap::new();
    for item in &module.items {
        match item {
            Item::Table(t) => {
                tables.insert(
                    t.name.node.clone(),
                    (
                        t.pk.iter().map(|c| c.node.clone()).collect(),
                        Vec::new(),
                    ),
                );
            }
            Item::Index(ix) => {
                if let Some((_, ixs)) = tables.get_mut(&ix.table.node) {
                    ixs.push((
                        ix.name.clone(),
                        ix.cols.iter().map(|c| c.node.clone()).collect(),
                    ));
                }
            }
            _ => {}
        }
    }
    let mut plans = HashMap::new();
    for item in &module.items {
        match item {
            Item::Const(c) => plan_expr(&c.value, &tables, &mut plans),
            Item::Operator(op) => {
                if let Some(b) = &op.body {
                    plan_expr(b, &tables, &mut plans);
                }
            }
            Item::Invariant(inv) => plan_expr(&inv.body, &tables, &mut plans),
            Item::Test(t) => {
                for s in &t.stmts {
                    match s {
                        TestStmt::Fixture { rows, .. } => plan_expr(rows, &tables, &mut plans),
                        TestStmt::Expect { lhs, rhs } => {
                            plan_expr(lhs, &tables, &mut plans);
                            plan_expr(rhs, &tables, &mut plans);
                        }
                    }
                }
            }
            Item::Property(p) => plan_temporal(&p.body, &tables, &mut plans),
            _ => {}
        }
    }
    OptimizedModule { desugared, plans }
}

fn plan_temporal(
    t: &TemporalExpr,
    tables: &HashMap<String, (Vec<String>, Vec<(Ident, Vec<String>)>)>,
    plans: &mut HashMap<Span, ReadPlan>,
) {
    match t {
        TemporalExpr::Always(inner) | TemporalExpr::Eventually(inner) => {
            plan_temporal(inner, tables, plans)
        }
        TemporalExpr::LeadsTo { lhs, rhs } | TemporalExpr::Until { lhs, rhs } => {
            plan_temporal(lhs, tables, plans);
            plan_temporal(rhs, tables, plans);
        }
        TemporalExpr::Primed(e) | TemporalExpr::State(e) => plan_expr(e, tables, plans),
    }
}

fn plan_expr(
    e: &Expr,
    tables: &HashMap<String, (Vec<String>, Vec<(Ident, Vec<String>)>)>,
    plans: &mut HashMap<Span, ReadPlan>,
) {
    if let ExprKind::ReadPrim { table, predicate } = &e.kind {
        let plan = classify(table, predicate, tables);
        plans.insert(e.span, plan);
    }
    crate::terminate::walk_children(e, &mut |child| plan_expr(child, tables, plans));
}

/// Classify one `read` by its predicate lambda.
fn classify(
    table: &Ident,
    predicate: &Expr,
    tables: &HashMap<String, (Vec<String>, Vec<(Ident, Vec<String>)>)>,
) -> ReadPlan {
    let ExprKind::Lambda(l) = &predicate.kind else {
        return ReadPlan::FullScan;
    };
    let [param] = l.params.as_slice() else {
        return ReadPlan::FullScan;
    };
    let PatternKind::Bind(row) = &param.pat.kind else {
        return ReadPlan::FullScan;
    };
    let row = row.node.as_str();
    // Unwrap the let chain left by block desugaring.
    let mut body: &Expr = &l.body;
    while let ExprKind::Let { body: b, .. } = &body.kind {
        body = b;
    }
    // Top-level `/\` decomposition.
    let mut conjuncts = Vec::new();
    split_ands(body, &mut conjuncts);
    // Columns constrained by a usable equality `row.c = e` (e row-free).
    let mut covered: HashSet<&str> = HashSet::new();
    for c in &conjuncts {
        if let Some(col) = usable_eq(c, row) {
            covered.insert(col);
        }
    }
    let Some((pk, indexes)) = tables.get(&table.node) else {
        return ReadPlan::FullScan;
    };
    if !pk.is_empty() && pk.iter().all(|c| covered.contains(c.as_str())) {
        return ReadPlan::PointLookup;
    }
    for (name, cols) in indexes {
        if !cols.is_empty() && cols.iter().all(|c| covered.contains(c.as_str())) {
            return ReadPlan::IndexScan {
                index: name.clone(),
            };
        }
    }
    ReadPlan::FullScan
}

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

/// A conjunct is a usable equality on column `c` when it has the shape
/// `row.c = e` or `e = row.c` with `e` not referencing the row variable.
fn usable_eq<'e>(e: &'e Expr, row: &str) -> Option<&'e str> {
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
                return Some(name.node.as_str());
            }
        }
    }
    None
}

/// Does `e` reference the row variable (respecting shadowing binders)?
fn refs_row(e: &Expr, row: &str) -> bool {
    fn go(e: &Expr, row: &str, shadowed: bool) -> bool {
        let binds_row = |pat: &Pattern| {
            pat.bound_idents().iter().any(|i| i.node == row)
        };
        match &e.kind {
            ExprKind::Var(u) => u.node == row && !shadowed,
            ExprKind::Let { pat, value, body } => {
                go(value, row, shadowed) || go(body, row, shadowed || binds_row(pat))
            }
            ExprKind::Lambda(l) => {
                let sh = shadowed || l.params.iter().any(|p| binds_row(&p.pat));
                go(&l.body, row, sh)
            }
            ExprKind::Match { scrutinee, arms } => {
                go(scrutinee, row, shadowed)
                    || arms
                        .iter()
                        .any(|a| go(&a.body, row, shadowed || binds_row(&a.pat)))
            }
            ExprKind::SetFilter { pat, source, pred } => {
                go(source, row, shadowed) || go(pred, row, shadowed || binds_row(pat))
            }
            ExprKind::SetMap { elem, gens } | ExprKind::BagMap { elem, gens } => {
                let mut sh = shadowed;
                for g in gens {
                    if go(&g.source, row, sh) {
                        return true;
                    }
                    sh = sh || binds_row(&g.pat);
                }
                go(elem, row, sh)
            }
            ExprKind::Quantifier { gens, body, .. } => {
                let mut sh = shadowed;
                for g in gens {
                    if go(&g.source, row, sh) {
                        return true;
                    }
                    sh = sh || binds_row(&g.pat);
                }
                go(body, row, sh)
            }
            _ => {
                let mut found = false;
                crate::terminate::walk_children(e, &mut |child| {
                    if !found && go(child, row, shadowed) {
                        found = true;
                    }
                });
                found
            }
        }
    }
    go(e, row, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar_module;

    fn optimize_ok(src: &str) -> OptimizedModule {
        let (typed, bag) = crate::lower::frontend(src);
        assert!(!bag.has_errors(), "{}", bag.render());
        optimize_module(desugar_module(typed.expect("typed module")))
    }

    fn plans_for(m: &OptimizedModule) -> Vec<&ReadPlan> {
        m.plans.values().collect()
    }

    #[test]
    fn pk_equality_is_point_lookup() {
        let m = optimize_ok(
            "module t;
             table users { id: int, name: string } primary key {id}
             query f(user_id: int) -> set<users> == {
                 read(users, lambda [user_id](u) { u.id = user_id })
             }",
        );
        assert_eq!(plans_for(&m), vec![&ReadPlan::PointLookup]);
    }

    #[test]
    fn lookup_desugars_to_point_lookup() {
        let m = optimize_ok(
            "module t;
             table users { id: int, name: string } primary key {id}
             query f(user_id: int) -> option<users> == {
                 lookup(users, user_id)
             }",
        );
        assert_eq!(plans_for(&m), vec![&ReadPlan::PointLookup]);
    }

    #[test]
    fn composite_pk_needs_all_columns() {
        let partial = optimize_ok(
            "module t;
             table edges { src: int, dst: int, w: int } primary key {src, dst}
             query f(s: int) -> set<edges> == {
                 read(edges, lambda [s](e) { e.src = s })
             }",
        );
        assert_eq!(plans_for(&partial), vec![&ReadPlan::FullScan]);

        let full = optimize_ok(
            "module t;
             table edges { src: int, dst: int, w: int } primary key {src, dst}
             query f(k: (int, int)) -> option<edges> == {
                 lookup(edges, k)
             }",
        );
        assert_eq!(plans_for(&full), vec![&ReadPlan::PointLookup]);
    }

    #[test]
    fn secondary_index_becomes_index_scan() {
        let m = optimize_ok(
            "module t;
             table sessions { session_id: int, user_id: int, duration: int } primary key {session_id}
             index sessions_by_user on sessions(user_id)
             query f(uid: int) -> set<sessions> == {
                 read(sessions, lambda [uid](s) { s.user_id = uid /\\ s.duration > 300 })
             }",
        );
        let plan = plans_for(&m);
        assert_eq!(plan.len(), 1);
        match plan[0] {
            ReadPlan::IndexScan { index } => assert_eq!(index.node, "sessions_by_user"),
            other => panic!("expected IndexScan, got {:?}", other),
        }
    }

    #[test]
    fn reversed_equality_and_row_dependent_rhs() {
        // `uid = s.user_id` (reversed) is usable ⇒ IndexScan.
        let m = optimize_ok(
            "module t;
             table sessions { session_id: int, user_id: int } primary key {session_id}
             index by_user on sessions(user_id)
             query f(uid: int) -> set<sessions> == {
                 read(sessions, lambda [uid](s) { uid = s.user_id })
             }",
        );
        assert!(matches!(plans_for(&m)[0], ReadPlan::IndexScan { .. }));

        // `s.user_id = s.session_id` references the row on both sides ⇒ FullScan.
        let m2 = optimize_ok(
            "module t;
             table sessions { session_id: int, user_id: int } primary key {session_id}
             index by_user on sessions(user_id)
             query f() -> set<sessions> == {
                 read(sessions, lambda(s) { s.user_id = s.session_id })
             }",
        );
        assert_eq!(plans_for(&m2), vec![&ReadPlan::FullScan]);
    }

    #[test]
    fn unconstrained_read_is_full_scan() {
        let m = optimize_ok(
            "module t;
             table users { id: int, active: bool } primary key {id}
             query f() -> set<users> == {
                 set { u \\in users if u.active }
             }",
        );
        // the desugared table-sugar read has predicate `true`
        assert_eq!(plans_for(&m), vec![&ReadPlan::FullScan]);
    }
}
