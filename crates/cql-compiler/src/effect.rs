//! Effect checking (doc/cql.md §3.7; pipeline §D.3) — a purely syntactic pass
//! over the resolved module.
//!
//! Rules:
//! - Operator levels: function = L0, query = L1, action = L2. Every call edge
//!   operator → operator must satisfy `callee.level ≤ caller.level` (a body
//!   may only call operators at its own level or lower; §3.7).
//! - Lambda bodies are always L0: no `ReadPrim`/`WriteCon`/`lookup`, and calls
//!   inside a lambda may only target `function`s (checked per call site).
//! - Read primitives (`read`/`lookup`/table-name sugar) may appear directly
//!   in query/action bodies only; write constructors (`insert`/`update`/
//!   `delete`) in action bodies only ("directly" = outside any lambda).
//! - `invariant`/`property` bodies are treated as L1; `const` bodies as L0;
//!   `test expect` bodies may call queries/actions (test-runner semantics).

use miette::NamedSource;

use crate::ast::*;
use crate::diag::{CqlError, DiagBag};
use crate::resolve::{Callee, ResolvedModule, VarRes};

/// Run the effect check on a resolved module. Errors abort the pass;
/// the pass currently emits no warnings but reserves the channel.
pub fn check_effects(resolved: &ResolvedModule) -> Result<(), DiagBag> {
    let src = NamedSource::new(format!("{}.cql", resolved.module.name.node), String::new());
    check_effects_with_src(resolved, src)
}

/// Like [`check_effects`] but attaches `src` to diagnostics.
pub fn check_effects_with_src(resolved: &ResolvedModule, src: NamedSource<String>) -> Result<(), DiagBag> {
    let mut c = Checker { resolved, diags: DiagBag::new(), src };
    for item in &resolved.module.items {
        match item {
            Item::Operator(o) => {
                if let Some(body) = &o.body {
                    let ctx = Ctx { edge_level: o.level, body_kind: BodyKind::Operator(o.level), in_lambda: false };
                    c.expr(body, ctx);
                }
            }
            Item::Const(cd) => {
                c.expr(&cd.value, Ctx::pure());
            }
            Item::Invariant(inv) => {
                c.expr(&inv.body, Ctx::query_like());
            }
            Item::Property(p) => {
                c.temporal(&p.body, Ctx::query_like());
            }
            Item::Test(t) => {
                for stmt in &t.stmts {
                    match stmt {
                        // Fixtures are plain data: only pure computation.
                        TestStmt::Fixture { rows, .. } => c.expr(rows, Ctx::pure()),
                        // expect may invoke queries/actions (test-runner semantics).
                        TestStmt::Expect { lhs, rhs } => {
                            let ctx = Ctx { edge_level: EffectLevel::Action, body_kind: BodyKind::TestExpect, in_lambda: false };
                            c.expr(lhs, ctx);
                            c.expr(rhs, ctx);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    c.diags.into_result(())
}

/// What kind of body is being checked (drives read/write permission).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Operator(EffectLevel),
    /// const / fixture data: pure.
    Pure,
    /// invariant / property: L1 (reads ok, no writes).
    QueryLike,
    /// test expect: calls up to L2 allowed, but no direct primitives/writes.
    TestExpect,
}

#[derive(Debug, Clone, Copy)]
struct Ctx {
    /// Caller level for the call-edge rule.
    edge_level: EffectLevel,
    body_kind: BodyKind,
    in_lambda: bool,
}

impl Ctx {
    fn pure() -> Self {
        Ctx { edge_level: EffectLevel::Function, body_kind: BodyKind::Pure, in_lambda: false }
    }

    fn query_like() -> Self {
        Ctx { edge_level: EffectLevel::Query, body_kind: BodyKind::QueryLike, in_lambda: false }
    }

    fn in_lambda(self) -> Self {
        Ctx { in_lambda: true, ..self }
    }

    /// Read primitives allowed: directly inside a query/action body or a
    /// query-like (invariant/property) body.
    fn allows_read(self) -> bool {
        if self.in_lambda {
            return false;
        }
        match self.body_kind {
            BodyKind::Operator(level) => level >= EffectLevel::Query,
            BodyKind::QueryLike => true,
            BodyKind::Pure | BodyKind::TestExpect => false,
        }
    }

    /// Write constructors allowed: directly inside an action body only.
    fn allows_write(self) -> bool {
        !self.in_lambda && matches!(self.body_kind, BodyKind::Operator(EffectLevel::Action))
    }
}

struct Checker<'a> {
    resolved: &'a ResolvedModule,
    diags: DiagBag,
    src: NamedSource<String>,
}

impl Checker<'_> {
    fn err(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_error(CqlError::new(self.src.clone(), span, message, help));
    }

    fn temporal(&mut self, t: &TemporalExpr, ctx: Ctx) {
        match t {
            TemporalExpr::Always(inner) | TemporalExpr::Eventually(inner) => self.temporal(inner, ctx),
            TemporalExpr::LeadsTo { lhs, rhs } | TemporalExpr::Until { lhs, rhs } => {
                self.temporal(lhs, ctx);
                self.temporal(rhs, ctx);
            }
            TemporalExpr::Primed(e) | TemporalExpr::State(e) => self.expr(e, ctx),
        }
    }

    fn expr(&mut self, e: &Expr, ctx: Ctx) {
        match &e.kind {
            ExprKind::Lit(_) | ExprKind::OptionNone => {}

            ExprKind::Var(name) => {
                // Table-name sugar in a generator/quantifier source is a read.
                if matches!(self.resolved.resolved.vars.get(&name.span), Some(VarRes::TableSugar))
                    && !ctx.allows_read()
                {
                    self.err(
                        name.span,
                        format!("table access `{}` is not allowed here", name.node),
                        Some("table reads may only appear directly in query/action bodies (or invariants/properties)".to_string()),
                    );
                }
            }

            ExprKind::ReadPrim { table, predicate } => {
                if !ctx.allows_read() {
                    self.err(
                        e.span,
                        if ctx.in_lambda {
                            "read primitive is not allowed inside a lambda".to_string()
                        } else {
                            format!("`read` on table `{}` is not allowed in this body", table.node)
                        },
                        Some("lambda bodies are always pure (L0); reads belong to query/action bodies (§3.7)".to_string()),
                    );
                }
                self.expr(predicate, ctx);
            }

            ExprKind::WriteCon(w) => {
                if !ctx.allows_write() {
                    self.err(
                        e.span,
                        if ctx.in_lambda {
                            "write constructor is not allowed inside a lambda".to_string()
                        } else {
                            "write constructors (`insert`/`update`/`delete`) are only allowed directly in action bodies".to_string()
                        },
                        None,
                    );
                }
                match w {
                    WriteCon::Insert { row, .. } => self.expr(row, ctx),
                    WriteCon::Update { key, transform, .. } => {
                        self.expr(key, ctx);
                        self.expr(transform, ctx);
                    }
                    WriteCon::Delete { key, .. } => self.expr(key, ctx),
                }
            }

            ExprKind::Call(call) => match self.resolved.resolved.callee.get(&call.name.span) {
                Some(Callee::LookupPrim) => {
                    if !ctx.allows_read() {
                        self.err(
                            call.name.span,
                            if ctx.in_lambda {
                                "read primitive `lookup` is not allowed inside a lambda".to_string()
                            } else {
                                "`lookup` is not allowed in this body".to_string()
                            },
                            Some("reads may only appear directly in query/action bodies (§3.7)".to_string()),
                        );
                    }
                    for a in &call.args {
                        self.expr(&a.value, ctx);
                    }
                }
                Some(Callee::Operator { name, level, .. }) => {
                    self.check_call_edge(&call.name, name, *level, ctx);
                    for a in &call.args {
                        self.expr(&a.value, ctx);
                    }
                }
                // Local function values, consts and stdlib calls are L0.
                _ => {
                    for a in &call.args {
                        self.expr(&a.value, ctx);
                    }
                }
            },

            ExprKind::Lambda(l) => {
                self.expr(&l.body, ctx.in_lambda());
            }

            ExprKind::Block { lets, tail } => {
                for l in lets {
                    self.expr(&l.value, ctx);
                }
                self.expr(tail, ctx);
            }
            ExprKind::Let { value, body, .. } => {
                self.expr(value, ctx);
                self.expr(body, ctx);
            }
            ExprKind::App { func, args } => {
                self.expr(func, ctx);
                for a in args {
                    self.expr(&a.value, ctx);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, ctx);
                for arm in arms {
                    self.expr(&arm.body, ctx);
                }
            }
            ExprKind::If { cond, then_br, else_br } => {
                self.expr(cond, ctx);
                self.expr(then_br, ctx);
                self.expr(else_br, ctx);
            }
            ExprKind::Try(inner) => self.expr(inner, ctx),
            ExprKind::RecordLit { fields } => {
                for f in fields {
                    self.expr(&f.value, ctx);
                }
            }
            ExprKind::RecordUpd { base, fields } => {
                self.expr(base, ctx);
                for f in fields {
                    self.expr(&f.value, ctx);
                }
            }
            ExprKind::Tuple(items) | ExprKind::Vector(items) | ExprKind::SetLiteral(items)
            | ExprKind::BagLiteral(items) => {
                for item in items {
                    self.expr(item, ctx);
                }
            }
            ExprKind::SetFilter { source, pred, .. } => {
                self.expr(source, ctx);
                self.expr(pred, ctx);
            }
            ExprKind::SetMap { elem, gens } | ExprKind::BagMap { elem, gens } => {
                for g in gens {
                    self.expr(&g.source, ctx);
                }
                self.expr(elem, ctx);
            }
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.expr(k, ctx);
                    self.expr(v, ctx);
                }
            }
            ExprKind::OptionSome(inner) => self.expr(inner, ctx),
            ExprKind::StrInterp(parts) => {
                for p in parts {
                    if let StrPart::Interp(inner) = p {
                        self.expr(inner, ctx);
                    }
                }
            }
            ExprKind::Quantifier { gens, body, .. } => {
                for g in gens {
                    self.expr(&g.source, ctx);
                }
                self.expr(body, ctx);
            }
            ExprKind::Cast { expr, .. } => self.expr(expr, ctx),
            ExprKind::BinOp { lhs, rhs, .. } => {
                self.expr(lhs, ctx);
                self.expr(rhs, ctx);
            }
            ExprKind::UnOp { operand, .. } => self.expr(operand, ctx),
            ExprKind::Primed(inner) => self.expr(inner, ctx),
            ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => self.expr(base, ctx),
            ExprKind::MethodCall { recv, args, .. } => {
                self.expr(recv, ctx);
                for a in args {
                    self.expr(&a.value, ctx);
                }
            }
            ExprKind::EnumConstruct { args, .. } => {
                for a in args {
                    self.expr(a, ctx);
                }
            }
        }
    }

    /// The call-edge rule (§3.7): a body may only call operators at its own
    /// level or lower; inside a lambda only L0 callees are allowed.
    fn check_call_edge(&mut self, name: &Ident, callee: &str, level: EffectLevel, ctx: Ctx) {
        if ctx.in_lambda {
            if level > EffectLevel::Function {
                self.err(
                    name.span,
                    format!("lambda body may only call `function`s, but `{}` is a {}", callee, level_name(level)),
                    Some("lambda bodies are always pure (L0) (§3.7)".to_string()),
                );
            }
            return;
        }
        if level > ctx.edge_level {
            self.err(
                name.span,
                format!(
                    "{} `{}` cannot be called from a {} body",
                    level_name(level),
                    callee,
                    level_name(ctx.edge_level)
                ),
                Some("a body may only call operators at its own effect level or lower (§3.7)".to_string()),
            );
        }
    }
}

fn level_name(level: EffectLevel) -> &'static str {
    match level {
        EffectLevel::Function => "function",
        EffectLevel::Query => "query",
        EffectLevel::Action => "action",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::{decl, expr, pat, ty};
    use crate::resolve::resolve_module;

    fn run(items: Vec<Item>) -> Result<(), DiagBag> {
        let r = resolve_module(decl::module("test", items), &[]).expect("resolve failed");
        check_effects(&r)
    }

    fn msgs(bag: &DiagBag) -> Vec<String> {
        bag.errors().iter().map(|e| e.message().to_string()).collect()
    }

    fn users_table() -> Item {
        decl::table("users", vec![("id", ty::int()), ("active", ty::bool_())], &["id"])
    }

    fn read_users() -> Expr {
        expr::call("read", vec![expr::var("users"), expr::lambda(&[], vec![pat::bind("u")], expr::bool_(true))])
    }

    #[test]
    fn function_calling_query_errors() {
        let items = vec![
            users_table(),
            decl::query("q", vec![], ty::int(), read_users()),
            decl::function("f", vec![], ty::int(), expr::call("q", vec![])),
        ];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("query `q` cannot be called from a function body")));
    }

    #[test]
    fn query_calling_action_errors() {
        let items = vec![
            users_table(),
            decl::action("a", vec![], expr::set_lit(vec![])),
            decl::query("q", vec![], ty::int(), expr::call("a", vec![])),
        ];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("action `a` cannot be called from a query body")));
    }

    #[test]
    fn read_inside_lambda_errors() {
        // query whose body is a lambda containing read: illegal (lambda is L0).
        let body = expr::lambda(&[], vec![pat::wild()], read_users());
        let items = vec![users_table(), decl::query("q", vec![], ty::int(), body)];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not allowed inside a lambda")));
    }

    #[test]
    fn lambda_calling_query_errors() {
        let lam = expr::lambda(&[], vec![pat::wild()], expr::call("q", vec![]));
        let items = vec![
            users_table(),
            decl::query("q", vec![], ty::int(), read_users()),
            decl::query("outer", vec![], ty::int(), lam),
        ];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("may only call `function`s")));
    }

    #[test]
    fn read_in_function_body_errors() {
        let items = vec![users_table(), decl::function("f", vec![], ty::int(), read_users())];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not allowed in this body")));
    }

    #[test]
    fn insert_in_action_ok_but_not_in_query() {
        let insert = expr::call(
            "insert",
            vec![expr::var("users"), expr::record_lit(vec![("id", expr::int(1)), ("active", expr::bool_(true))])],
        );
        run(vec![users_table(), decl::action("a", vec![], insert.clone())])
            .expect("insert in action is legal");

        let bag = run(vec![users_table(), decl::query("q", vec![], ty::int(), insert)]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("only allowed directly in action bodies")));
    }

    #[test]
    fn query_calling_query_ok_and_action_calling_all_ok() {
        let items = vec![
            users_table(),
            decl::query("q1", vec![], ty::int(), read_users()),
            decl::query("q2", vec![], ty::int(), expr::call("q1", vec![])),
            decl::action("a", vec![], expr::call("q2", vec![])),
            decl::function("f", vec![], ty::int(), expr::int(1)),
            decl::query("q3", vec![], ty::int(), expr::call("f", vec![])),
        ];
        run(items).expect("level-preserving/rising edges are legal");
    }

    #[test]
    fn invariant_is_query_like_and_const_is_pure() {
        // invariant with read: ok
        let inv = Item::Invariant(InvariantDecl {
            name: crate::ast::builder::id("inv"),
            table: crate::ast::builder::id("users"),
            body: read_users(),
        });
        run(vec![users_table(), inv]).expect("invariant may read");

        // const with read: error
        let c = decl::const_("c", ty::int(), read_users());
        let bag = run(vec![users_table(), c]).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("not allowed in this body")));

        // invariant calling an action: error (edge rule, L1 caller)
        let items = vec![
            users_table(),
            decl::action("a", vec![], expr::set_lit(vec![])),
            Item::Invariant(InvariantDecl {
                name: crate::ast::builder::id("inv2"),
                table: crate::ast::builder::id("users"),
                body: expr::call("a", vec![]),
            }),
        ];
        let bag = run(items).unwrap_err();
        assert!(msgs(&bag).iter().any(|m| m.contains("cannot be called from a query body")));
    }

    #[test]
    fn test_expect_may_call_action() {
        let items = vec![
            users_table(),
            decl::action("a", vec![], expr::set_lit(vec![])),
            Item::Test(TestDecl {
                name: crate::ast::builder::id("t"),
                stmts: vec![TestStmt::Expect {
                    lhs: expr::call("a", vec![]),
                    rhs: expr::set_lit(vec![]),
                }],
            }),
        ];
        run(items).expect("test expect may call actions");
    }
}
