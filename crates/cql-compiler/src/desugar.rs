//! Desugaring: surface AST → core AST (doc/cql.md §D.2).
//!
//! Consumes a [`TypedModule`] and rewrites every expression of its module in
//! place, eliminating the surface-only nodes:
//!
//! - `Block` → nested `Let`
//! - `SetFilter` / `SetMap` / `BagMap` → `fold` over `to_vector` (bag sources
//!   go through `bag_to_set` first; option sources become a `match`, §4.4.1)
//! - `Quantifier` → `fold` (short-circuiting is a runtime/codegen concern)
//! - `Try` (`e?`) → `match e { some(v) => B[v], none => none }`, innermost
//!   first, never lifting past a lambda / comprehension-body boundary
//! - `StrInterp` → `concat` chain with `to_string_*` picked from `expr_tys`
//! - table-name sugar (`Var` resolved as `TableSugar`) → `read(t, λ(_){true})`
//! - `lookup(t, k)` → `let __key = k in only(read(t, λ[__key](row){ ... }))`
//!   with one equality per primary-key column (tuple projection for
//!   composite keys), conjoined
//! - `MethodCall` → `Call` / `App`, re-dispatched via `expr_tys` exactly as
//!   the type checker did
//!
//! Synthesized nodes reuse the span of the expression they replace. The
//! desugarer is total: it only runs after the frontend passes reported no
//! errors, so missing side-table entries are unreachable in practice and fall
//! back to leaving the node untouched.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ast::*;
use crate::resolve::{stdlib_signature, Callee, ResolvedModule, Resolutions, VarRes};
use crate::types::{Ty, TypedModule};

/// The output of desugaring: the same typed module with a core-only AST.
#[derive(Debug, Clone, PartialEq)]
pub struct DesugaredModule {
    pub typed: TypedModule,
}

/// Desugar a typed module (doc/cql.md §D.2).
pub fn desugar_module(typed: TypedModule) -> DesugaredModule {
    let TypedModule {
        resolved,
        expr_tys,
        instantiations,
        operator_sigs,
        operator_locals,
    } = typed;
    let ResolvedModule {
        mut module,
        resolved,
    } = resolved;
    let mut d = Desugarer::new(&expr_tys, &resolved, &module);
    d.module(&mut module);
    DesugaredModule {
        typed: TypedModule {
            resolved: ResolvedModule { module, resolved },
            expr_tys,
            instantiations,
            operator_sigs,
            operator_locals,
        },
    }
}

struct Desugarer<'a> {
    expr_tys: &'a HashMap<Span, Ty>,
    resolutions: &'a Resolutions,
    /// Names of module-level `function`s (for method-call dispatch, A.3).
    fn_names: HashSet<String>,
    /// Table name → primary-key columns (for `lookup` desugaring).
    tables: HashMap<String, Vec<String>>,
    fresh: u32,
}

/// An expression with the `?`-induced matches it hoists into the enclosing
/// scope. `matches` are outermost-first: collapsing wraps them around `core`.
struct Hoisted {
    matches: Vec<(Expr, Ident, Span)>,
    core: Expr,
}

/// Shape of a comprehension / quantifier generator source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SrcKind {
    Set,
    Bag,
    Vector,
    Option,
}

impl<'a> Desugarer<'a> {
    fn new(expr_tys: &'a HashMap<Span, Ty>, resolutions: &'a Resolutions, module: &Module) -> Self {
        let mut fn_names = HashSet::new();
        let mut tables = HashMap::new();
        for item in &module.items {
            match item {
                Item::Operator(op) if op.level == EffectLevel::Function => {
                    fn_names.insert(op.name.node.clone());
                }
                Item::Table(t) => {
                    tables.insert(
                        t.name.node.clone(),
                        t.pk.iter().map(|c| c.node.clone()).collect(),
                    );
                }
                _ => {}
            }
        }
        Desugarer {
            expr_tys,
            resolutions,
            fn_names,
            tables,
            fresh: 0,
        }
    }

    fn fresh_ident(&mut self, tag: &str, span: Span) -> Ident {
        self.fresh += 1;
        Ident::new(format!("__desugar_{}_{}", tag, self.fresh), span)
    }

    fn ty_of(&self, span: Span) -> Option<&Ty> {
        self.expr_tys.get(&span)
    }

    // ---- module driver -------------------------------------------------------

    fn module(&mut self, m: &mut Module) {
        let items = std::mem::take(&mut m.items);
        m.items = items
            .into_iter()
            .map(|item| match item {
                Item::Const(mut c) => {
                    c.value = self.body(c.value);
                    Item::Const(c)
                }
                Item::Operator(mut op) => {
                    if let Some(b) = op.body {
                        op.body = Some(self.body(b));
                    }
                    Item::Operator(op)
                }
                Item::Invariant(mut inv) => {
                    inv.body = self.body(inv.body);
                    Item::Invariant(inv)
                }
                Item::Test(mut t) => {
                    t.stmts = t
                        .stmts
                        .into_iter()
                        .map(|s| match s {
                            TestStmt::Fixture { table, rows } => TestStmt::Fixture {
                                table,
                                rows: self.body(rows),
                            },
                            TestStmt::Expect { lhs, rhs } => TestStmt::Expect {
                                lhs: self.body(lhs),
                                rhs: self.body(rhs),
                            },
                        })
                        .collect();
                    Item::Test(t)
                }
                Item::Property(mut p) => {
                    p.body = self.temporal(p.body);
                    Item::Property(p)
                }
                other => other,
            })
            .collect();
    }

    fn temporal(&mut self, t: TemporalExpr) -> TemporalExpr {
        match t {
            TemporalExpr::Always(inner) => TemporalExpr::Always(Box::new(self.temporal(*inner))),
            TemporalExpr::Eventually(inner) => TemporalExpr::Eventually(Box::new(self.temporal(*inner))),
            TemporalExpr::LeadsTo { lhs, rhs } => TemporalExpr::LeadsTo {
                lhs: Box::new(self.temporal(*lhs)),
                rhs: Box::new(self.temporal(*rhs)),
            },
            TemporalExpr::Until { lhs, rhs } => TemporalExpr::Until {
                lhs: Box::new(self.temporal(*lhs)),
                rhs: Box::new(self.temporal(*rhs)),
            },
            TemporalExpr::Primed(e) => TemporalExpr::Primed(self.body(e)),
            TemporalExpr::State(e) => TemporalExpr::State(self.body(e)),
        }
    }

    // ---- scopes --------------------------------------------------------------

    /// Desugar a scope body: recursive desugar, then lift `?` into matches.
    fn body(&mut self, e: Expr) -> Expr {
        let d = self.expr(e);
        let h = self.elim(d);
        self.collapse(h)
    }

    fn collapse(&mut self, h: Hoisted) -> Expr {
        let mut out = h.core;
        for (scrut, var, span) in h.matches.into_iter().rev() {
            out = Expr::new(
                ExprKind::Match {
                    scrutinee: Box::new(scrut),
                    arms: vec![
                        MatchArm {
                            pat: Pattern::new(
                                PatternKind::Some(Box::new(Pattern::new(
                                    PatternKind::Bind(var),
                                    span,
                                ))),
                                span,
                            ),
                            body: out,
                        },
                        MatchArm {
                            pat: Pattern::new(PatternKind::None, span),
                            body: Expr::new(ExprKind::OptionNone, span),
                        },
                    ],
                },
                span,
            );
        }
        out
    }

    // ---- recursive desugar (Try-preserving) ----------------------------------

    fn expr(&mut self, e: Expr) -> Expr {
        let span = e.span;
        let kind = match e.kind {
            ExprKind::Lit(_) | ExprKind::OptionNone => e.kind,
            ExprKind::Var(name) => {
                if self.resolutions.vars.get(&name.span) == Some(&VarRes::TableSugar) {
                    // Table-name sugar: `t` ⇒ `read(t, λ(row){ true })`.
                    let row = self.fresh_ident("row", span);
                    let pred = self.mk_lambda(
                        vec![Pattern::new(PatternKind::Bind(row), span)],
                        Expr::new(ExprKind::Lit(Literal::Bool(true)), span),
                        span,
                    );
                    ExprKind::ReadPrim {
                        table: name,
                        predicate: Box::new(pred),
                    }
                } else {
                    ExprKind::Var(name)
                }
            }
            ExprKind::Block { lets, tail } => {
                let mut out = self.expr(*tail);
                for l in lets.into_iter().rev() {
                    out = Expr::new(
                        ExprKind::Let {
                            pat: l.pat,
                            value: Box::new(self.expr(l.value)),
                            body: Box::new(out),
                        },
                        span,
                    );
                }
                return Expr::new(out.kind, span);
            }
            ExprKind::Let { pat, value, body } => ExprKind::Let {
                pat,
                value: Box::new(self.expr(*value)),
                body: Box::new(self.expr(*body)),
            },
            ExprKind::Lambda(l) => ExprKind::Lambda(Lambda {
                captures: l.captures,
                params: l.params,
                ret: l.ret,
                body: Box::new(self.body(*l.body)),
            }),
            ExprKind::App { func, args } => ExprKind::App {
                func: Box::new(self.expr(*func)),
                args: self.args(args),
            },
            ExprKind::Call(call) => {
                if self.resolutions.callee.get(&call.name.span) == Some(&Callee::LookupPrim) {
                    return self.desugar_lookup(call, span);
                }
                ExprKind::Call(Call {
                    name: call.name,
                    type_args: call.type_args,
                    args: self.args(call.args),
                })
            }
            ExprKind::Match { scrutinee, arms } => ExprKind::Match {
                scrutinee: Box::new(self.expr(*scrutinee)),
                arms: arms
                    .into_iter()
                    .map(|a| MatchArm {
                        pat: a.pat,
                        body: self.expr(a.body),
                    })
                    .collect(),
            },
            ExprKind::If {
                cond,
                then_br,
                else_br,
            } => ExprKind::If {
                cond: Box::new(self.expr(*cond)),
                then_br: Box::new(self.expr(*then_br)),
                else_br: Box::new(self.expr(*else_br)),
            },
            ExprKind::Try(inner) => ExprKind::Try(Box::new(self.expr(*inner))),
            ExprKind::RecordLit { fields } => ExprKind::RecordLit {
                fields: self.field_inits(fields),
            },
            ExprKind::RecordUpd { base, fields } => ExprKind::RecordUpd {
                base: Box::new(self.expr(*base)),
                fields: self.field_inits(fields),
            },
            ExprKind::Tuple(es) => ExprKind::Tuple(self.exprs(es)),
            ExprKind::Vector(es) => ExprKind::Vector(self.exprs(es)),
            ExprKind::SetLiteral(es) => ExprKind::SetLiteral(self.exprs(es)),
            ExprKind::BagLiteral(es) => ExprKind::BagLiteral(self.exprs(es)),
            ExprKind::MapLit(entries) => ExprKind::MapLit(
                entries
                    .into_iter()
                    .map(|(k, v)| (self.expr(k), self.expr(v)))
                    .collect(),
            ),
            ExprKind::OptionSome(inner) => ExprKind::OptionSome(Box::new(self.expr(*inner))),
            ExprKind::SetFilter { pat, source, pred } => {
                return self.desugar_set_filter(pat, *source, *pred, span);
            }
            ExprKind::SetMap { elem, gens } => {
                return self.desugar_map_comp(*elem, gens, span, Coll::Set);
            }
            ExprKind::BagMap { elem, gens } => {
                return self.desugar_map_comp(*elem, gens, span, Coll::Bag);
            }
            ExprKind::StrInterp(parts) => return self.desugar_interp(parts, span),
            ExprKind::Quantifier { kind, gens, body } => {
                return self.desugar_quantifier(kind, gens, *body, span);
            }
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: Box::new(self.expr(*expr)),
                ty,
            },
            ExprKind::BinOp { op, lhs, rhs } => ExprKind::BinOp {
                op,
                lhs: Box::new(self.expr(*lhs)),
                rhs: Box::new(self.expr(*rhs)),
            },
            ExprKind::UnOp { op, operand } => ExprKind::UnOp {
                op,
                operand: Box::new(self.expr(*operand)),
            },
            ExprKind::Field { base, name } => ExprKind::Field {
                base: Box::new(self.expr(*base)),
                name,
            },
            ExprKind::TupleProj { base, index } => ExprKind::TupleProj {
                base: Box::new(self.expr(*base)),
                index,
            },
            ExprKind::MethodCall { recv, name, args } => {
                return self.desugar_method_call(*recv, name, args, span);
            }
            ExprKind::Primed(inner) => ExprKind::Primed(Box::new(self.expr(*inner))),
            ExprKind::ReadPrim { table, predicate } => ExprKind::ReadPrim {
                table,
                predicate: Box::new(self.expr(*predicate)),
            },
            ExprKind::WriteCon(w) => ExprKind::WriteCon(self.write_con(w)),
            ExprKind::EnumConstruct { name, args } => ExprKind::EnumConstruct {
                name,
                args: self.exprs(args),
            },
        };
        Expr::new(kind, span)
    }

    fn exprs(&mut self, es: Vec<Expr>) -> Vec<Expr> {
        es.into_iter().map(|e| self.expr(e)).collect()
    }

    fn args(&mut self, args: Vec<Arg>) -> Vec<Arg> {
        args.into_iter()
            .map(|a| Arg {
                name: a.name,
                value: self.expr(a.value),
            })
            .collect()
    }

    fn field_inits(&mut self, fields: Vec<FieldInit>) -> Vec<FieldInit> {
        fields
            .into_iter()
            .map(|f| FieldInit {
                name: f.name,
                value: self.expr(f.value),
            })
            .collect()
    }

    fn write_con(&mut self, w: WriteCon) -> WriteCon {
        match w {
            WriteCon::Insert { table, row } => WriteCon::Insert {
                table,
                row: Box::new(self.expr(*row)),
            },
            WriteCon::Update {
                table,
                key,
                transform,
            } => WriteCon::Update {
                table,
                key: Box::new(self.expr(*key)),
                transform: Box::new(self.expr(*transform)),
            },
            WriteCon::Delete { table, key } => WriteCon::Delete {
                table,
                key: Box::new(self.expr(*key)),
            },
        }
    }

    // ---- comprehensions & quantifiers -----------------------------------------

    fn src_kind(&self, span: Span) -> SrcKind {
        match self.ty_of(span) {
            Some(Ty::Set(_)) => SrcKind::Set,
            Some(Ty::Bag(_)) => SrcKind::Bag,
            Some(Ty::Option(_)) => SrcKind::Option,
            _ => SrcKind::Vector,
        }
    }

    /// `fold`-ready iteration vector for a generator source.
    fn as_vector(&mut self, src: Expr, span: Span) -> Expr {
        match self.src_kind(src.span) {
            SrcKind::Set => self.std_call("to_vector", vec![src], span),
            SrcKind::Bag => {
                let s = self.std_call("bag_to_set", vec![src], span);
                self.std_call("to_vector", vec![s], span)
            }
            _ => src,
        }
    }

    /// Build the nested `fold` (or `match`, for option sources) over `gens`,
    /// threading the accumulator; `k` produces the innermost body given the
    /// current accumulator expression.
    fn fold_gens(
        &mut self,
        mut gens: std::vec::IntoIter<Generator>,
        span: Span,
        acc0: Expr,
        k: &mut dyn FnMut(&mut Self, Expr) -> Expr,
    ) -> Expr {
        match gens.next() {
            None => k(self, acc0),
            Some(g) => {
                let src = self.expr(g.source);
                if self.src_kind(src.span) == SrcKind::Option {
                    // §4.4.1: an option source iterates 0 or 1 elements.
                    let some_br = self.fold_gens(gens, span, acc0.clone(), k);
                    Expr::new(
                        ExprKind::Match {
                            scrutinee: Box::new(src),
                            arms: vec![
                                MatchArm {
                                    pat: Pattern::new(
                                        PatternKind::Some(Box::new(g.pat)),
                                        span,
                                    ),
                                    body: some_br,
                                },
                                MatchArm {
                                    pat: Pattern::new(PatternKind::None, span),
                                    body: acc0,
                                },
                            ],
                        },
                        span,
                    )
                } else {
                    let acc = self.fresh_ident("acc", span);
                    let acc_var = Expr::new(ExprKind::Var(acc.clone()), span);
                    let body = self.fold_gens(gens, span, acc_var, k);
                    let lam = self.mk_lambda(
                        vec![
                            Pattern::new(PatternKind::Bind(acc), span),
                            g.pat,
                        ],
                        body,
                        span,
                    );
                    let vec_src = self.as_vector(src, span);
                    self.std_call("fold", vec![vec_src, acc0, lam], span)
                }
            }
        }
    }

    fn desugar_set_filter(&mut self, pat: Pattern, source: Expr, pred: Expr, span: Span) -> Expr {
        let pred = self.body(pred);
        // The singleton element expression: the bound variable itself for a
        // plain binder; otherwise bind the value to a fresh variable and
        // destructure it inside the fold body.
        let (gen_pat, elem, wrap) = match &pat.kind {
            PatternKind::Bind(x) => (
                pat.clone(),
                Expr::new(ExprKind::Var(x.clone()), x.span),
                None,
            ),
            _ => {
                let v = self.fresh_ident("elem", span);
                (
                    Pattern::new(PatternKind::Bind(v.clone()), span),
                    Expr::new(ExprKind::Var(v), span),
                    Some(pat),
                )
            }
        };
        let mut k = move |_d: &mut Self, acc: Expr| {
            let inner = Expr::new(
                ExprKind::If {
                    cond: Box::new(pred.clone()),
                    then_br: Box::new(Expr::new(
                        ExprKind::BinOp {
                            op: BinOpKind::Cup,
                            lhs: Box::new(acc.clone()),
                            rhs: Box::new(Expr::new(
                                ExprKind::SetLiteral(vec![elem.clone()]),
                                span,
                            )),
                        },
                        span,
                    )),
                    else_br: Box::new(acc),
                },
                span,
            );
            match &wrap {
                Some(p) => Expr::new(
                    ExprKind::Let {
                        pat: p.clone(),
                        value: Box::new(elem.clone()),
                        body: Box::new(inner),
                    },
                    span,
                ),
                None => inner,
            }
        };
        let empty = Expr::new(ExprKind::SetLiteral(vec![]), span);
        let gens = vec![Generator {
            pat: gen_pat,
            source,
        }]
        .into_iter();
        self.fold_gens(gens, span, empty, &mut k)
    }

    fn desugar_map_comp(&mut self, elem: Expr, gens: Vec<Generator>, span: Span, coll: Coll) -> Expr {
        let elem = self.body(elem);
        let mut k = move |d: &mut Self, acc: Expr| match coll {
            Coll::Set => Expr::new(
                ExprKind::BinOp {
                    op: BinOpKind::Cup,
                    lhs: Box::new(acc),
                    rhs: Box::new(Expr::new(ExprKind::SetLiteral(vec![elem.clone()]), span)),
                },
                span,
            ),
            Coll::Bag => d.std_call(
                "bag_union",
                vec![
                    acc,
                    Expr::new(ExprKind::BagLiteral(vec![elem.clone()]), span),
                ],
                span,
            ),
        };
        let empty = Expr::new(
            match coll {
                Coll::Set => ExprKind::SetLiteral(vec![]),
                Coll::Bag => ExprKind::BagLiteral(vec![]),
            },
            span,
        );
        self.fold_gens(gens.into_iter(), span, empty, &mut k)
    }

    fn desugar_quantifier(
        &mut self,
        kind: QuantKind,
        gens: Vec<Generator>,
        body: Expr,
        span: Span,
    ) -> Expr {
        let body = self.body(body);
        let (op, init) = match kind {
            QuantKind::Forall => (BinOpKind::And, true),
            QuantKind::Exists => (BinOpKind::Or, false),
        };
        let mut k = move |_d: &mut Self, acc: Expr| {
            Expr::new(
                ExprKind::BinOp {
                    op,
                    lhs: Box::new(acc),
                    rhs: Box::new(body.clone()),
                },
                span,
            )
        };
        let acc0 = Expr::new(ExprKind::Lit(Literal::Bool(init)), span);
        self.fold_gens(gens.into_iter(), span, acc0, &mut k)
    }

    // ---- string interpolation --------------------------------------------------

    fn desugar_interp(&mut self, parts: Vec<StrPart>, span: Span) -> Expr {
        let mut out: Option<Expr> = None;
        for p in parts {
            let piece = match p {
                StrPart::Lit(s) => Expr::new(ExprKind::Lit(Literal::Str(s)), span),
                StrPart::Interp(e) => {
                    let d = self.expr(e);
                    let name = match self.ty_of(d.span) {
                        Some(Ty::Int) => Some("to_string_int"),
                        Some(Ty::Float) => Some("to_string_float"),
                        Some(Ty::Decimal(_)) => Some("to_string_decimal"),
                        Some(Ty::Bool) => Some("to_string_bool"),
                        Some(Ty::Date) => Some("to_string_date"),
                        _ => None, // string: identity
                    };
                    match name {
                        Some(f) => self.std_call(f, vec![d], span),
                        None => d,
                    }
                }
            };
            out = Some(match out {
                None => piece,
                Some(l) => self.std_call("concat", vec![l, piece], span),
            });
        }
        out.unwrap_or_else(|| Expr::new(ExprKind::Lit(Literal::Str(String::new())), span))
    }

    // ---- lookup & table sugar ----------------------------------------------------

    /// `lookup(t, k)` ⇒ `let __key = k in only(read(t, λ[__key](row){ ⋀ row.pkᵢ = __key[.i] }))`.
    fn desugar_lookup(&mut self, call: Call, span: Span) -> Expr {
        let mut args = call.args.into_iter();
        let table = match args.next().map(|a| a.value) {
            Some(Expr {
                kind: ExprKind::Var(t),
                ..
            }) => t,
            _ => {
                // Unreachable after a clean frontend; keep the call as-is.
                return Expr::new(
                    ExprKind::Call(Call {
                        name: call.name,
                        type_args: call.type_args,
                        args: args.collect(),
                    }),
                    span,
                );
            }
        };
        let key = match args.next() {
            Some(a) => self.expr(a.value),
            None => {
                return Expr::new(ExprKind::Var(table), span);
            }
        };
        let pk = match self.tables.get(&table.node) {
            Some(pk) if !pk.is_empty() => pk.clone(),
            _ => {
                return Expr::new(ExprKind::Var(table), span);
            }
        };
        let k = self.fresh_ident("key", span);
        let row = self.fresh_ident("row", span);
        // ⋀ row.pkᵢ = key-projᵢ
        let mut pred_body: Option<Expr> = None;
        for (i, col) in pk.iter().enumerate() {
            let lhs = Expr::new(
                ExprKind::Field {
                    base: Box::new(Expr::new(ExprKind::Var(row.clone()), span)),
                    name: Ident::new(col.clone(), span),
                },
                span,
            );
            let rhs = if pk.len() == 1 {
                Expr::new(ExprKind::Var(k.clone()), span)
            } else {
                Expr::new(
                    ExprKind::TupleProj {
                        base: Box::new(Expr::new(ExprKind::Var(k.clone()), span)),
                        index: i as u32,
                    },
                    span,
                )
            };
            let eq = Expr::new(
                ExprKind::BinOp {
                    op: BinOpKind::Eq,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
            pred_body = Some(match pred_body {
                None => eq,
                Some(l) => Expr::new(
                    ExprKind::BinOp {
                        op: BinOpKind::And,
                        lhs: Box::new(l),
                        rhs: Box::new(eq),
                    },
                    span,
                ),
            });
        }
        let pred = self.mk_lambda(
            vec![Pattern::new(PatternKind::Bind(row), span)],
            pred_body.expect("pk is non-empty"),
            span,
        );
        let read = Expr::new(
            ExprKind::ReadPrim {
                table,
                predicate: Box::new(pred),
            },
            span,
        );
        let only = self.std_call("only", vec![read], span);
        Expr::new(
            ExprKind::Let {
                pat: Pattern::new(PatternKind::Bind(k), span),
                value: Box::new(key),
                body: Box::new(only),
            },
            span,
        )
    }

    // ---- method calls -------------------------------------------------------------

    fn desugar_method_call(&mut self, recv: Expr, name: Ident, args: Vec<Arg>, span: Span) -> Expr {
        let recv = self.expr(recv);
        let args = self.args(args);
        self.dispatch_method(recv, name, args, span)
    }

    fn dispatch_method(&mut self, recv: Expr, name: Ident, args: Vec<Arg>, span: Span) -> Expr {
        // A `let` produced by `lookup` desugaring hoists out of the receiver.
        if let ExprKind::Let { pat, value, body } = recv.kind {
            let inner = self.dispatch_method(*body, name, args, span);
            return Expr::new(
                ExprKind::Let {
                    pat,
                    value,
                    body: Box::new(inner),
                },
                span,
            );
        }
        let recv_ty = self.ty_of(recv.span).cloned();
        // 1. A function-typed record field wins (application, not a call).
        if let Some(Ty::Record(fs)) = &recv_ty {
            if let Some((_, fty)) = fs.iter().find(|(n, _)| n == &name.node) {
                if matches!(fty, Ty::Fun(..)) {
                    return Expr::new(
                        ExprKind::App {
                            func: Box::new(Expr::new(
                                ExprKind::Field {
                                    base: Box::new(recv),
                                    name,
                                },
                                span,
                            )),
                            args,
                        },
                        span,
                    );
                }
            }
        }
        // 2. A module-level function shadows the stdlib (A.3).
        // 3. A stdlib function with the receiver as first argument.
        let mut prepend = vec![Arg {
            name: None,
            value: recv,
        }];
        prepend.extend(args);
        if self.fn_names.contains(&name.node) || stdlib_signature(&name.node).is_some() {
            return Expr::new(
                ExprKind::Call(Call {
                    name,
                    type_args: None,
                    args: prepend,
                }),
                span,
            );
        }
        // 4. `m.get(k)` etc. on a map receiver falls back to `map_<name>` (§4.10).
        if matches!(recv_ty, Some(Ty::Map(..))) {
            let prefixed = format!("map_{}", name.node);
            if stdlib_signature(&prefixed).is_some() {
                return Expr::new(
                    ExprKind::Call(Call {
                        name: Ident::new(prefixed, name.span),
                        type_args: None,
                        args: prepend,
                    }),
                    span,
                );
            }
        }
        // Unreachable after a clean type check; reconstruct a best-effort call.
        Expr::new(
            ExprKind::Call(Call {
                name,
                type_args: None,
                args: prepend,
            }),
            span,
        )
    }

    // ---- try elimination ------------------------------------------------------------

    /// Lift every `?` in `e` (already desugared) into hoisted matches.
    /// Traversal is left-to-right so nested matches preserve evaluation order;
    /// lambda / read-predicate boundaries are collapsed internally and never
    /// hoisted past.
    fn elim(&mut self, e: Expr) -> Hoisted {
        let span = e.span;
        let none = |core| Hoisted {
            matches: vec![],
            core,
        };
        match e.kind {
            ExprKind::Try(inner) => {
                let h = self.elim(*inner);
                let v = self.fresh_ident("try", span);
                let mut matches = h.matches;
                matches.push((h.core, v.clone(), span));
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::Var(v), span),
                }
            }
            ExprKind::Let { pat, value, body } => {
                let hv = self.elim(*value);
                let hb = self.elim(*body);
                let mut matches = hv.matches;
                matches.extend(hb.matches);
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::Let {
                            pat,
                            value: Box::new(hv.core),
                            body: Box::new(hb.core),
                        },
                        span,
                    ),
                }
            }
            ExprKind::Lambda(l) => {
                // Scope boundary: eliminate inside, hoist nothing out.
                let h = self.elim(*l.body);
                let body = self.collapse(h);
                none(Expr::new(
                    ExprKind::Lambda(Lambda {
                        captures: l.captures,
                        params: l.params,
                        ret: l.ret,
                        body: Box::new(body),
                    }),
                    span,
                ))
            }
            ExprKind::App { func, args } => {
                let hf = self.elim(*func);
                let (mut matches, args) = self.elim_args(args);
                let mut all = hf.matches;
                all.append(&mut matches);
                Hoisted {
                    matches: all,
                    core: Expr::new(
                        ExprKind::App {
                            func: Box::new(hf.core),
                            args,
                        },
                        span,
                    ),
                }
            }
            ExprKind::Call(c) => {
                let (matches, args) = self.elim_args(c.args);
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::Call(Call {
                            name: c.name,
                            type_args: c.type_args,
                            args,
                        }),
                        span,
                    ),
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let hs = self.elim(*scrutinee);
                let mut matches = hs.matches;
                let mut new_arms = Vec::with_capacity(arms.len());
                for a in arms {
                    let h = self.elim(a.body);
                    matches.extend(h.matches);
                    new_arms.push(MatchArm { pat: a.pat, body: h.core });
                }
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::Match {
                            scrutinee: Box::new(hs.core),
                            arms: new_arms,
                        },
                        span,
                    ),
                }
            }
            ExprKind::If {
                cond,
                then_br,
                else_br,
            } => {
                let hc = self.elim(*cond);
                let ht = self.elim(*then_br);
                let he = self.elim(*else_br);
                let mut matches = hc.matches;
                matches.extend(ht.matches);
                matches.extend(he.matches);
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::If {
                            cond: Box::new(hc.core),
                            then_br: Box::new(ht.core),
                            else_br: Box::new(he.core),
                        },
                        span,
                    ),
                }
            }
            ExprKind::RecordLit { fields } => {
                let (matches, fields) = self.elim_field_inits(fields);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::RecordLit { fields }, span),
                }
            }
            ExprKind::RecordUpd { base, fields } => {
                let hb = self.elim(*base);
                let (mut m2, fields) = self.elim_field_inits(fields);
                let mut matches = hb.matches;
                matches.append(&mut m2);
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::RecordUpd {
                            base: Box::new(hb.core),
                            fields,
                        },
                        span,
                    ),
                }
            }
            ExprKind::Tuple(es) => {
                let (matches, es) = self.elim_exprs(es);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::Tuple(es), span),
                }
            }
            ExprKind::Vector(es) => {
                let (matches, es) = self.elim_exprs(es);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::Vector(es), span),
                }
            }
            ExprKind::SetLiteral(es) => {
                let (matches, es) = self.elim_exprs(es);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::SetLiteral(es), span),
                }
            }
            ExprKind::BagLiteral(es) => {
                let (matches, es) = self.elim_exprs(es);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::BagLiteral(es), span),
                }
            }
            ExprKind::MapLit(entries) => {
                let mut matches = Vec::new();
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let hk = self.elim(k);
                    let hv = self.elim(v);
                    matches.extend(hk.matches);
                    matches.extend(hv.matches);
                    out.push((hk.core, hv.core));
                }
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::MapLit(out), span),
                }
            }
            ExprKind::OptionSome(inner) => {
                let h = self.elim(*inner);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(ExprKind::OptionSome(Box::new(h.core)), span),
                }
            }
            ExprKind::Cast { expr, ty } => {
                let h = self.elim(*expr);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(
                        ExprKind::Cast {
                            expr: Box::new(h.core),
                            ty,
                        },
                        span,
                    ),
                }
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let hl = self.elim(*lhs);
                let hr = self.elim(*rhs);
                let mut matches = hl.matches;
                matches.extend(hr.matches);
                Hoisted {
                    matches,
                    core: Expr::new(
                        ExprKind::BinOp {
                            op,
                            lhs: Box::new(hl.core),
                            rhs: Box::new(hr.core),
                        },
                        span,
                    ),
                }
            }
            ExprKind::UnOp { op, operand } => {
                let h = self.elim(*operand);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(
                        ExprKind::UnOp {
                            op,
                            operand: Box::new(h.core),
                        },
                        span,
                    ),
                }
            }
            ExprKind::Field { base, name } => {
                let h = self.elim(*base);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(
                        ExprKind::Field {
                            base: Box::new(h.core),
                            name,
                        },
                        span,
                    ),
                }
            }
            ExprKind::TupleProj { base, index } => {
                let h = self.elim(*base);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(
                        ExprKind::TupleProj {
                            base: Box::new(h.core),
                            index,
                        },
                        span,
                    ),
                }
            }
            ExprKind::Primed(inner) => {
                let h = self.elim(*inner);
                Hoisted {
                    matches: h.matches,
                    core: Expr::new(ExprKind::Primed(Box::new(h.core)), span),
                }
            }
            ExprKind::ReadPrim { table, predicate } => {
                // The predicate is a lambda: a scope boundary.
                let h = self.elim(*predicate);
                let p = self.collapse(h);
                none(Expr::new(
                    ExprKind::ReadPrim {
                        table,
                        predicate: Box::new(p),
                    },
                    span,
                ))
            }
            ExprKind::WriteCon(w) => {
                let (matches, w) = self.elim_write_con(w);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::WriteCon(w), span),
                }
            }
            ExprKind::EnumConstruct { name, args } => {
                let (matches, args) = self.elim_exprs(args);
                Hoisted {
                    matches,
                    core: Expr::new(ExprKind::EnumConstruct { name, args }, span),
                }
            }
            // Leaves and anything unexpected: nothing to lift.
            other => none(Expr::new(other, span)),
        }
    }

    fn elim_exprs(&mut self, es: Vec<Expr>) -> (Vec<(Expr, Ident, Span)>, Vec<Expr>) {
        let mut matches = Vec::new();
        let mut out = Vec::with_capacity(es.len());
        for e in es {
            let h = self.elim(e);
            matches.extend(h.matches);
            out.push(h.core);
        }
        (matches, out)
    }

    fn elim_args(&mut self, args: Vec<Arg>) -> (Vec<(Expr, Ident, Span)>, Vec<Arg>) {
        let mut matches = Vec::new();
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let h = self.elim(a.value);
            matches.extend(h.matches);
            out.push(Arg {
                name: a.name,
                value: h.core,
            });
        }
        (matches, out)
    }

    fn elim_field_inits(
        &mut self,
        fields: Vec<FieldInit>,
    ) -> (Vec<(Expr, Ident, Span)>, Vec<FieldInit>) {
        let mut matches = Vec::new();
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            let h = self.elim(f.value);
            matches.extend(h.matches);
            out.push(FieldInit {
                name: f.name,
                value: h.core,
            });
        }
        (matches, out)
    }

    fn elim_write_con(&mut self, w: WriteCon) -> (Vec<(Expr, Ident, Span)>, WriteCon) {
        match w {
            WriteCon::Insert { table, row } => {
                let h = self.elim(*row);
                (
                    h.matches,
                    WriteCon::Insert {
                        table,
                        row: Box::new(h.core),
                    },
                )
            }
            WriteCon::Update {
                table,
                key,
                transform,
            } => {
                let h = self.elim(*key);
                let ht = self.elim(*transform);
                let t = self.collapse(ht);
                (
                    h.matches,
                    WriteCon::Update {
                        table,
                        key: Box::new(h.core),
                        transform: Box::new(t),
                    },
                )
            }
            WriteCon::Delete { table, key } => {
                let h = self.elim(*key);
                (
                    h.matches,
                    WriteCon::Delete {
                        table,
                        key: Box::new(h.core),
                    },
                )
            }
        }
    }

    // ---- helpers --------------------------------------------------------------------

    fn std_call(&mut self, name: &str, args: Vec<Expr>, span: Span) -> Expr {
        Expr::new(
            ExprKind::Call(Call {
                name: Ident::new(name.to_string(), span),
                type_args: None,
                args: args
                    .into_iter()
                    .map(|value| Arg { name: None, value })
                    .collect(),
            }),
            span,
        )
    }

    /// Build a lambda, computing the capture list as the body's free local
    /// variables not bound by the parameters.
    fn mk_lambda(&mut self, params: Vec<Pattern>, body: Expr, span: Span) -> Expr {
        let mut bound: HashSet<String> = HashSet::new();
        for p in &params {
            for id in p.bound_idents() {
                bound.insert(id.node.clone());
            }
        }
        let mut captures: Vec<Ident> = Vec::new();
        self.collect_free(&body, &mut bound, &mut captures);
        Expr::new(
            ExprKind::Lambda(Lambda {
                captures,
                params: params
                    .into_iter()
                    .map(|pat| LambdaParam { pat, ty: None })
                    .collect(),
                ret: None,
                body: Box::new(body),
            }),
            span,
        )
    }

    /// Collect free variables that refer to outer *local* bindings (candidates
    /// for a synthesized lambda's capture list). Vars resolved to anything
    /// else (constants, functions, stdlib, table sugar) are globals.
    fn collect_free(&self, e: &Expr, bound: &HashSet<String>, out: &mut Vec<Ident>) {
        match &e.kind {
            ExprKind::Var(id) => {
                let local = matches!(
                    self.resolutions.vars.get(&id.span),
                    Some(VarRes::Local) | None
                );
                if local
                    && !bound.contains(&id.node)
                    && !out.iter().any(|o| o.node == id.node)
                {
                    out.push(id.clone());
                }
            }
            ExprKind::Let { pat, value, body } => {
                self.collect_free(value, bound, out);
                self.with_bound(pat, bound, |bound| {
                    self.collect_free(body, bound, out);
                });
            }
            ExprKind::Lambda(l) => {
                let mut bound2 = bound.clone();
                for p in &l.params {
                    for id in p.pat.bound_idents() {
                        bound2.insert(id.node.clone());
                    }
                }
                self.collect_free(&l.body, &mut bound2, out);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_free(scrutinee, bound, out);
                self.collect_free_arms(arms, bound, out);
            }
            ExprKind::SetFilter { pat, source, pred } => {
                self.collect_free(source, bound, out);
                self.with_bound(pat, bound, |bound| self.collect_free(pred, bound, out));
            }
            ExprKind::SetMap { elem, gens } | ExprKind::BagMap { elem, gens } => {
                let mut bound2 = bound.clone();
                for g in gens {
                    self.collect_free(&g.source, &bound2, out);
                    for id in g.pat.bound_idents() {
                        bound2.insert(id.node.clone());
                    }
                }
                self.collect_free(elem, &mut bound2, out);
            }
            ExprKind::Quantifier { gens, body, .. } => {
                let mut bound2 = bound.clone();
                for g in gens {
                    self.collect_free(&g.source, &bound2, out);
                    for id in g.pat.bound_idents() {
                        bound2.insert(id.node.clone());
                    }
                }
                self.collect_free(body, &mut bound2, out);
            }
            ExprKind::ReadPrim { predicate, .. } => self.collect_free(predicate, bound, out),
            ExprKind::WriteCon(w) => match w {
                WriteCon::Insert { row, .. } => self.collect_free(row, bound, out),
                WriteCon::Update { key, transform, .. } => {
                    self.collect_free(key, bound, out);
                    self.collect_free(transform, bound, out);
                }
                WriteCon::Delete { key, .. } => self.collect_free(key, bound, out),
            },
            ExprKind::App { func, args } => {
                self.collect_free(func, bound, out);
                for a in args {
                    self.collect_free(&a.value, bound, out);
                }
            }
            ExprKind::Call(c) => {
                for a in &c.args {
                    self.collect_free(&a.value, bound, out);
                }
            }
            ExprKind::If {
                cond,
                then_br,
                else_br,
            } => {
                self.collect_free(cond, bound, out);
                self.collect_free(then_br, bound, out);
                self.collect_free(else_br, bound, out);
            }
            ExprKind::Try(inner)
            | ExprKind::OptionSome(inner)
            | ExprKind::Primed(inner)
            | ExprKind::UnOp { operand: inner, .. } => self.collect_free(inner, bound, out),
            ExprKind::Cast { expr, .. } => self.collect_free(expr, bound, out),
            ExprKind::RecordLit { fields } => {
                for f in fields {
                    self.collect_free(&f.value, bound, out);
                }
            }
            ExprKind::RecordUpd { base, fields } => {
                self.collect_free(base, bound, out);
                for f in fields {
                    self.collect_free(&f.value, bound, out);
                }
            }
            ExprKind::Tuple(es)
            | ExprKind::Vector(es)
            | ExprKind::SetLiteral(es)
            | ExprKind::BagLiteral(es) => {
                for x in es {
                    self.collect_free(x, bound, out);
                }
            }
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.collect_free(k, bound, out);
                    self.collect_free(v, bound, out);
                }
            }
            ExprKind::BinOp { lhs, rhs, .. } => {
                self.collect_free(lhs, bound, out);
                self.collect_free(rhs, bound, out);
            }
            ExprKind::Field { base, .. } | ExprKind::TupleProj { base, .. } => {
                self.collect_free(base, bound, out)
            }
            ExprKind::MethodCall { recv, args, .. } => {
                self.collect_free(recv, bound, out);
                for a in args {
                    self.collect_free(&a.value, bound, out);
                }
            }
            ExprKind::StrInterp(parts) => {
                for p in parts {
                    if let StrPart::Interp(x) = p {
                        self.collect_free(x, bound, out);
                    }
                }
            }
            ExprKind::Block { lets, tail } => {
                let mut bound2 = bound.clone();
                for l in lets {
                    self.collect_free(&l.value, &mut bound2, out);
                    for id in l.pat.bound_idents() {
                        bound2.insert(id.node.clone());
                    }
                }
                self.collect_free(tail, &mut bound2, out);
            }
            ExprKind::EnumConstruct { args, .. } => {
                for x in args {
                    self.collect_free(x, bound, out);
                }
            }
            ExprKind::Lit(_) | ExprKind::OptionNone => {}
        }
    }

    fn collect_free_arms(&self, arms: &[MatchArm], bound: &HashSet<String>, out: &mut Vec<Ident>) {
        for a in arms {
            let mut bound2 = bound.clone();
            for id in a.pat.bound_idents() {
                bound2.insert(id.node.clone());
            }
            self.collect_free(&a.body, &mut bound2, out);
        }
    }

    fn with_bound<R>(
        &self,
        pat: &Pattern,
        bound: &HashSet<String>,
        f: impl FnOnce(&mut HashSet<String>) -> R,
    ) -> R {
        let mut bound2 = bound.clone();
        for id in pat.bound_idents() {
            bound2.insert(id.node.clone());
        }
        f(&mut bound2)
    }
}

/// Collection kind for map-comprehension desugaring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coll {
    Set,
    Bag,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desugar_ok(src: &str) -> DesugaredModule {
        let (typed, bag) = crate::lower::frontend(src);
        assert!(!bag.has_errors(), "{}", bag.render());
        desugar_module(typed.expect("typed module"))
    }

    fn op_body<'a>(m: &'a DesugaredModule, name: &str) -> &'a Expr {
        for item in &m.typed.resolved.module.items {
            if let Item::Operator(op) = item {
                if op.name.node == name {
                    return op.body.as_ref().expect("operator body");
                }
            }
        }
        panic!("operator {} not found", name)
    }

    fn is_call(e: &Expr, name: &str) -> bool {
        matches!(&e.kind, ExprKind::Call(c) if c.name.node == name)
    }

    fn call_args<'a>(e: &'a Expr) -> &'a [Arg] {
        match &e.kind {
            ExprKind::Call(c) => &c.args,
            other => panic!("expected Call, got {:?}", other),
        }
    }

    #[test]
    fn block_becomes_nested_let() {
        let m = desugar_ok(
            "module t;
             function f(a: int) -> int == {
                 let x == a + 1;
                 let y == x + 1;
                 y
             }",
        );
        let b = op_body(&m, "f");
        let ExprKind::Let { pat, body, .. } = &b.kind else {
            panic!("expected Let, got {:?}", b.kind)
        };
        assert!(matches!(pat.kind, PatternKind::Bind(ref x) if x.node == "x"));
        let ExprKind::Let { pat: p2, body: b2, .. } = &body.kind else {
            panic!("expected nested Let, got {:?}", body.kind)
        };
        assert!(matches!(p2.kind, PatternKind::Bind(ref y) if y.node == "y"));
        assert!(matches!(b2.kind, ExprKind::Var(ref v) if v.node == "y"));
    }

    #[test]
    fn set_filter_becomes_fold() {
        let m = desugar_ok(
            "module t;
             query f(s: set<int>) -> set<int> == {
                 set { x \\in s if x > 0 }
             }",
        );
        let b = op_body(&m, "f");
        assert!(is_call(b, "fold"), "got {:?}", b.kind);
        let args = call_args(b);
        assert!(is_call(&args[0].value, "to_vector"));
        assert!(matches!(args[1].value.kind, ExprKind::SetLiteral(ref es) if es.is_empty()));
        let ExprKind::Lambda(l) = &args[2].value.kind else {
            panic!("expected lambda")
        };
        assert_eq!(l.params.len(), 2);
        let ExprKind::If {
            then_br, else_br, ..
        } = &l.body.kind
        else {
            panic!("expected if in fold body, got {:?}", l.body.kind)
        };
        // then: acc \cup set {x}; else: acc
        let ExprKind::BinOp { op: BinOpKind::Cup, rhs, .. } = &then_br.kind else {
            panic!("expected cup, got {:?}", then_br.kind)
        };
        assert!(matches!(rhs.kind, ExprKind::SetLiteral(ref es) if es.len() == 1));
        assert!(matches!(else_br.kind, ExprKind::Var(_)));
    }

    #[test]
    fn set_map_becomes_fold_with_cup() {
        let m = desugar_ok(
            "module t;
             function f(s: set<int>) -> set<int> == {
                 set { x * 2 : x \\in s }
             }",
        );
        let b = op_body(&m, "f");
        assert!(is_call(b, "fold"));
        let args = call_args(b);
        let ExprKind::Lambda(l) = &args[2].value.kind else { panic!("lambda") };
        let ExprKind::BinOp { op: BinOpKind::Cup, rhs, .. } = &l.body.kind else {
            panic!("expected cup, got {:?}", l.body.kind)
        };
        let ExprKind::SetLiteral(es) = &rhs.kind else { panic!("singleton set") };
        assert!(matches!(es[0].kind, ExprKind::BinOp { op: BinOpKind::Mul, .. }));
    }

    #[test]
    fn bag_map_becomes_fold_with_bag_union_and_bag_to_set() {
        let m = desugar_ok(
            "module t;
             function f(s: bag<int>) -> bag<int> == {
                 bag { x + 1 : x \\in s }
             }",
        );
        let b = op_body(&m, "f");
        assert!(is_call(b, "fold"));
        let args = call_args(b);
        // bag source ⇒ to_vector(bag_to_set(s))
        assert!(is_call(&args[0].value, "to_vector"));
        let inner = &call_args(&args[0].value)[0].value;
        assert!(is_call(inner, "bag_to_set"));
        assert!(matches!(args[1].value.kind, ExprKind::BagLiteral(ref es) if es.is_empty()));
        let ExprKind::Lambda(l) = &args[2].value.kind else { panic!("lambda") };
        assert!(is_call(&l.body, "bag_union"), "got {:?}", l.body.kind);
    }

    #[test]
    fn forall_becomes_fold_with_and() {
        let m = desugar_ok(
            "module t;
             function f(s: set<int>) -> bool == {
                 \\A x \\in s : x > 0
             }",
        );
        let b = op_body(&m, "f");
        assert!(is_call(b, "fold"));
        let args = call_args(b);
        assert!(matches!(args[1].value.kind, ExprKind::Lit(Literal::Bool(true))));
        let ExprKind::Lambda(l) = &args[2].value.kind else { panic!("lambda") };
        assert!(matches!(l.body.kind, ExprKind::BinOp { op: BinOpKind::And, .. }));
    }

    #[test]
    fn exists_over_option_source_becomes_match() {
        let m = desugar_ok(
            "module t;
             function f(o: option<int>) -> bool == {
                 \\E x \\in o : x > 0
             }",
        );
        let b = op_body(&m, "f");
        let ExprKind::Match { arms, .. } = &b.kind else {
            panic!("expected match, got {:?}", b.kind)
        };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[0].pat.kind, PatternKind::Some(_)));
        // some(x) ⇒ false \/ x > 0
        assert!(matches!(arms[0].body.kind, ExprKind::BinOp { op: BinOpKind::Or, .. }));
        assert!(matches!(arms[1].pat.kind, PatternKind::None));
        assert!(matches!(arms[1].body.kind, ExprKind::Lit(Literal::Bool(false))));
    }

    #[test]
    fn try_lifts_to_match_innermost_first() {
        let m = desugar_ok(
            "module t;
             function f(a: option<int>, b: option<int>) -> option<int> == {
                 some(a? + b?)
             }",
        );
        let b = op_body(&m, "f");
        // match a { some(v1) => match b { some(v2) => v1 + v2, none => none }, none => none }
        let ExprKind::Match { arms, .. } = &b.kind else {
            panic!("expected outer match, got {:?}", b.kind)
        };
        assert!(matches!(arms[1].pat.kind, PatternKind::None));
        assert!(matches!(arms[1].body.kind, ExprKind::OptionNone));
        let ExprKind::Match { arms: inner, .. } = &arms[0].body.kind else {
            panic!("expected inner match, got {:?}", arms[0].body.kind)
        };
        let ExprKind::OptionSome(sum) = &inner[0].body.kind else {
            panic!("expected some, got {:?}", inner[0].body.kind)
        };
        assert!(matches!(sum.kind, ExprKind::BinOp { op: BinOpKind::Add, .. }));
        assert!(matches!(inner[1].body.kind, ExprKind::OptionNone));
    }

    #[test]
    fn try_inside_lambda_stays_inside() {
        let m = desugar_ok(
            "module t;
             function f(o: option<int>) -> option<int> == {
                 let g == lambda [o](x: int) -> option<int> { some(o?) };
                 some(1)
             }",
        );
        let b = op_body(&m, "f");
        // The outer body must NOT be a match: `o?` belongs to the lambda.
        let ExprKind::Let { value, body, .. } = &b.kind else {
            panic!("expected let, got {:?}", b.kind)
        };
        let ExprKind::Lambda(l) = &value.kind else { panic!("lambda") };
        assert!(matches!(l.body.kind, ExprKind::Match { .. }));
        assert!(matches!(body.kind, ExprKind::OptionSome(_)));
    }

    #[test]
    fn string_interp_becomes_concat_chain() {
        let m = desugar_ok(
            "module t;
             function f(n: int) -> string == {
                 \"n = \\(n)!\"
             }",
        );
        let b = op_body(&m, "f");
        // concat(concat("n = ", to_string_int(n)), "!")
        assert!(is_call(b, "concat"), "got {:?}", b.kind);
        let args = call_args(b);
        assert!(is_call(&args[0].value, "concat"));
        let inner = call_args(&args[0].value);
        assert!(matches!(inner[0].value.kind, ExprKind::Lit(Literal::Str(_))));
        assert!(is_call(&inner[1].value, "to_string_int"));
        assert!(matches!(args[1].value.kind, ExprKind::Lit(Literal::Str(_))));
    }

    #[test]
    fn lookup_becomes_only_read_with_pk_predicate() {
        let m = desugar_ok(
            "module t;
             table users { id: int, name: string } primary key {id}
             query f(user_id: int) -> option<string> == {
                 lookup(users, user_id).map(lambda(u) { u.name })
             }",
        );
        let b = op_body(&m, "f");
        // let __key = user_id in map(only(read(users, λ[__key](row){ row.id = __key })), λ(u){ u.name })
        let ExprKind::Let { value, body, .. } = &b.kind else {
            panic!("expected key let, got {:?}", b.kind)
        };
        assert!(matches!(value.kind, ExprKind::Var(ref v) if v.node == "user_id"));
        // method call on the result dispatched to stdlib `map`
        assert!(is_call(body, "map"), "got {:?}", body.kind);
        let only = &call_args(body)[0].value;
        assert!(is_call(only, "only"), "got {:?}", only.kind);
        let ExprKind::ReadPrim { table, predicate } = &call_args(only)[0].value.kind else {
            panic!("expected read prim")
        };
        assert_eq!(table.node, "users");
        let ExprKind::Lambda(l) = &predicate.kind else { panic!("lambda") };
        assert_eq!(l.captures.len(), 1, "key capture");
        let ExprKind::BinOp { op: BinOpKind::Eq, lhs, .. } = &l.body.kind else {
            panic!("expected pk equality, got {:?}", l.body.kind)
        };
        let ExprKind::Field { name, .. } = &lhs.kind else { panic!("field") };
        assert_eq!(name.node, "id");
    }

    #[test]
    fn composite_key_lookup_conjoins_tuple_projections() {
        let m = desugar_ok(
            "module t;
             table edges { src: int, dst: int, w: int } primary key {src, dst}
             query f(k: (int, int)) -> option<edges> == {
                 lookup(edges, k)
             }",
        );
        let b = op_body(&m, "f");
        let ExprKind::Let { body, .. } = &b.kind else { panic!("let") };
        assert!(is_call(body, "only"));
        let ExprKind::ReadPrim { predicate, .. } = &call_args(body)[0].value.kind else {
            panic!("read prim")
        };
        let ExprKind::Lambda(l) = &predicate.kind else { panic!("lambda") };
        // row.src = __key.0 /\ row.dst = __key.1
        let ExprKind::BinOp { op: BinOpKind::And, lhs, rhs } = &l.body.kind else {
            panic!("expected conjunction, got {:?}", l.body.kind)
        };
        for side in [lhs, rhs] {
            let ExprKind::BinOp { op: BinOpKind::Eq, rhs: proj, .. } = &side.kind else {
                panic!("expected equality")
            };
            assert!(matches!(proj.kind, ExprKind::TupleProj { .. }));
        }
    }

    #[test]
    fn table_sugar_becomes_read_prim() {
        let m = desugar_ok(
            "module t;
             table users { id: int } primary key {id}
             query f() -> set<users> == { set { u \\in users if u.id > 0 } }",
        );
        let b = op_body(&m, "f");
        assert!(is_call(b, "fold"), "got {:?}", b.kind);
        let tv = &call_args(b)[0].value;
        assert!(is_call(tv, "to_vector"));
        let ExprKind::ReadPrim { table, predicate } = &call_args(tv)[0].value.kind else {
            panic!("expected read prim")
        };
        assert_eq!(table.node, "users");
        let ExprKind::Lambda(l) = &predicate.kind else { panic!("lambda") };
        assert!(l.captures.is_empty());
        assert!(matches!(l.body.kind, ExprKind::Lit(Literal::Bool(true))));
    }

    #[test]
    fn no_surface_nodes_remain() {
        let m = desugar_ok(
            "module t;
             table users { id: int, name: string, active: bool } primary key {id}
             query f(user_id: int) -> option<string> == {
                 let u == lookup(users, user_id)?;
                 some(\"hello \\(u.name)\")
             }
             query g() -> bool == {
                 \\A u \\in set { v \\in users if v.active } : u.name /= \"\"
             }",
        );
        fn walk(e: &Expr) {
            match &e.kind {
                ExprKind::Block { .. }
                | ExprKind::Try(_)
                | ExprKind::SetFilter { .. }
                | ExprKind::SetMap { .. }
                | ExprKind::BagMap { .. }
                | ExprKind::StrInterp(_)
                | ExprKind::Quantifier { .. }
                | ExprKind::MethodCall { .. } => panic!("surface node remains: {:?}", e.kind),
                _ => {}
            }
            crate::terminate::walk_children(e, &mut walk);
        }
        for item in &m.typed.resolved.module.items {
            if let Item::Operator(op) = item {
                walk(op.body.as_ref().unwrap());
            }
        }
    }
}
