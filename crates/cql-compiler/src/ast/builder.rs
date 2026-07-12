//! Test-only AST construction helpers.
//!
//! Each helper assigns a fresh, unique dummy span so that span-keyed side
//! tables (e.g. `resolve::Resolutions`) work in hand-built trees. Intended
//! for unit tests of compiler passes; not part of the public API surface.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

static NEXT: AtomicU32 = AtomicU32::new(1);

/// A fresh unique dummy span (distinct from `Span::new_dummy()`).
pub fn sp() -> Span {
    let n = NEXT.fetch_add(2, Ordering::Relaxed);
    Span { start: n, end: n + 1 }
}

pub fn id(name: &str) -> Ident {
    Spanned::new(name.to_string(), sp())
}

pub mod pat {
    use super::*;

    pub fn p(kind: PatternKind) -> Pattern {
        Pattern::new(kind, sp())
    }

    pub fn wild() -> Pattern {
        p(PatternKind::Wildcard)
    }

    pub fn bind(name: &str) -> Pattern {
        p(PatternKind::Bind(id(name)))
    }

    pub fn lit_int(v: i64) -> Pattern {
        p(PatternKind::Lit(PatLit::Int(v)))
    }

    pub fn lit_str(s: &str) -> Pattern {
        p(PatternKind::Lit(PatLit::Str(s.to_string())))
    }

    pub fn lit_bool(v: bool) -> Pattern {
        p(PatternKind::Lit(PatLit::Bool(v)))
    }

    pub fn some(inner: Pattern) -> Pattern {
        p(PatternKind::Some(Box::new(inner)))
    }

    pub fn none() -> Pattern {
        p(PatternKind::None)
    }

    pub fn variant(name: &str, args: Vec<Pattern>) -> Pattern {
        p(PatternKind::Variant { name: id(name), args })
    }

    pub fn tuple(pats: Vec<Pattern>) -> Pattern {
        p(PatternKind::Tuple(pats))
    }

    pub fn record(names: &[&str]) -> Pattern {
        p(PatternKind::Record(names.iter().map(|n| id(n)).collect()))
    }

    pub fn cons_nil() -> Pattern {
        p(PatternKind::ConsNil)
    }

    pub fn cons(head: Pattern, tail: Pattern) -> Pattern {
        p(PatternKind::Cons { head: Box::new(head), tail: Box::new(tail) })
    }
}

pub mod ty {
    use super::*;

    pub fn t(kind: TypeKind) -> Type {
        Type::new(kind, sp())
    }

    pub fn int() -> Type {
        t(TypeKind::Int)
    }

    pub fn bool_() -> Type {
        t(TypeKind::Bool)
    }

    pub fn float() -> Type {
        t(TypeKind::Float)
    }

    pub fn string() -> Type {
        t(TypeKind::String)
    }

    pub fn named(name: &str) -> Type {
        t(TypeKind::Named { name: id(name), args: vec![] })
    }

    pub fn decimal(precision: Option<(u32, u32)>) -> Type {
        t(TypeKind::Decimal(precision))
    }

    pub fn date() -> Type {
        t(TypeKind::Date)
    }

    pub fn key(table: &str) -> Type {
        t(TypeKind::Key(id(table)))
    }

    pub fn value(table: &str) -> Type {
        t(TypeKind::Value(id(table)))
    }

    pub fn option(inner: Type) -> Type {
        t(TypeKind::Option(Box::new(inner)))
    }

    pub fn vector(inner: Type) -> Type {
        t(TypeKind::Vector(Box::new(inner)))
    }

    pub fn set(inner: Type) -> Type {
        t(TypeKind::Set(Box::new(inner)))
    }

    pub fn bag(inner: Type) -> Type {
        t(TypeKind::Bag(Box::new(inner)))
    }

    pub fn map(k: Type, v: Type) -> Type {
        t(TypeKind::Map(Box::new(k), Box::new(v)))
    }

    pub fn tuple(items: Vec<Type>) -> Type {
        t(TypeKind::Tuple(items))
    }

    pub fn record(fields: Vec<(&str, Type)>) -> Type {
        t(TypeKind::Record(fields.into_iter().map(|(n, t)| (id(n), t)).collect()))
    }

    pub fn fun(arg: Type, ret: Type) -> Type {
        t(TypeKind::Fun(Box::new(arg), Box::new(ret)))
    }
}

pub mod expr {
    use super::*;

    pub fn e(kind: ExprKind) -> Expr {
        Expr::new(kind, sp())
    }

    pub fn int(v: i64) -> Expr {
        e(ExprKind::Lit(Literal::Int(v)))
    }

    pub fn float(v: f64) -> Expr {
        e(ExprKind::Lit(Literal::Float(v)))
    }

    pub fn bool_(v: bool) -> Expr {
        e(ExprKind::Lit(Literal::Bool(v)))
    }

    pub fn str_(s: &str) -> Expr {
        e(ExprKind::Lit(Literal::Str(s.to_string())))
    }

    pub fn var(name: &str) -> Expr {
        e(ExprKind::Var(id(name)))
    }

    pub fn arg(value: Expr) -> Arg {
        Arg { name: None, value }
    }

    pub fn named_arg(name: &str, value: Expr) -> Arg {
        Arg { name: Some(id(name)), value }
    }

    pub fn call(name: &str, args: Vec<Expr>) -> Expr {
        call_args(name, args.into_iter().map(arg).collect())
    }

    pub fn call_args(name: &str, args: Vec<Arg>) -> Expr {
        e(ExprKind::Call(Call { name: id(name), type_args: None, args }))
    }

    pub fn app(func: Expr, args: Vec<Expr>) -> Expr {
        e(ExprKind::App { func: Box::new(func), args: args.into_iter().map(arg).collect() })
    }

    pub fn lambda(captures: &[&str], params: Vec<Pattern>, body: Expr) -> Expr {
        e(ExprKind::Lambda(Lambda {
            captures: captures.iter().map(|c| id(c)).collect(),
            params: params.into_iter().map(|pat| LambdaParam { pat, ty: None }).collect(),
            ret: None,
            body: Box::new(body),
        }))
    }

    pub fn block(lets: Vec<LetStmt>, tail: Expr) -> Expr {
        e(ExprKind::Block { lets, tail: Box::new(tail) })
    }

    pub fn let_(pat: Pattern, value: Expr) -> LetStmt {
        LetStmt { pat, ty: None, value }
    }

    pub fn match_(scrutinee: Expr, arms: Vec<(Pattern, Expr)>) -> Expr {
        e(ExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms: arms.into_iter().map(|(pat, body)| MatchArm { pat, body }).collect(),
        })
    }

    pub fn if_(cond: Expr, then_br: Expr, else_br: Expr) -> Expr {
        e(ExprKind::If {
            cond: Box::new(cond),
            then_br: Box::new(then_br),
            else_br: Box::new(else_br),
        })
    }

    pub fn binop(op: BinOpKind, lhs: Expr, rhs: Expr) -> Expr {
        e(ExprKind::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) })
    }

    pub fn field(base: Expr, name: &str) -> Expr {
        e(ExprKind::Field { base: Box::new(base), name: id(name) })
    }

    pub fn tuple_proj(base: Expr, index: u32) -> Expr {
        e(ExprKind::TupleProj { base: Box::new(base), index })
    }

    pub fn tuple(items: Vec<Expr>) -> Expr {
        e(ExprKind::Tuple(items))
    }

    pub fn vector(items: Vec<Expr>) -> Expr {
        e(ExprKind::Vector(items))
    }

    pub fn set_lit(items: Vec<Expr>) -> Expr {
        e(ExprKind::SetLiteral(items))
    }

    pub fn gen(pat: Pattern, source: Expr) -> Generator {
        Generator { pat, source }
    }

    pub fn set_filter(pat: Pattern, source: Expr, pred: Expr) -> Expr {
        e(ExprKind::SetFilter { pat, source: Box::new(source), pred: Box::new(pred) })
    }

    pub fn set_map(elem: Expr, gens: Vec<Generator>) -> Expr {
        e(ExprKind::SetMap { elem: Box::new(elem), gens })
    }

    pub fn quant(kind: QuantKind, gens: Vec<Generator>, body: Expr) -> Expr {
        e(ExprKind::Quantifier { kind, gens, body: Box::new(body) })
    }

    pub fn record_lit(fields: Vec<(&str, Expr)>) -> Expr {
        e(ExprKind::RecordLit {
            fields: fields.into_iter().map(|(n, v)| FieldInit { name: id(n), value: v }).collect(),
        })
    }

    pub fn some(inner: Expr) -> Expr {
        e(ExprKind::OptionSome(Box::new(inner)))
    }

    pub fn none() -> Expr {
        e(ExprKind::OptionNone)
    }

    pub fn decimal(repr: &str, precision: Option<(u32, u32)>) -> Expr {
        e(ExprKind::Lit(Literal::Decimal { repr: repr.to_string(), precision }))
    }

    pub fn date(year: i32, month: u8, day: u8) -> Expr {
        e(ExprKind::Lit(Literal::Date { year, month, day }))
    }

    pub fn try_(inner: Expr) -> Expr {
        e(ExprKind::Try(Box::new(inner)))
    }

    pub fn cast(inner: Expr, ty: Type) -> Expr {
        e(ExprKind::Cast { expr: Box::new(inner), ty })
    }

    pub fn unop(op: UnOpKind, operand: Expr) -> Expr {
        e(ExprKind::UnOp { op, operand: Box::new(operand) })
    }

    pub fn method_call(recv: Expr, name: &str, args: Vec<Expr>) -> Expr {
        e(ExprKind::MethodCall {
            recv: Box::new(recv),
            name: id(name),
            args: args.into_iter().map(arg).collect(),
        })
    }

    pub fn call_ty(name: &str, type_args: Vec<Type>, args: Vec<Expr>) -> Expr {
        e(ExprKind::Call(Call {
            name: id(name),
            type_args: Some(type_args),
            args: args.into_iter().map(arg).collect(),
        }))
    }

    /// A lambda with per-parameter optional annotations and optional return
    /// annotation.
    pub fn lambda_ann(
        captures: &[&str],
        params: Vec<(Pattern, Option<Type>)>,
        ret: Option<Type>,
        body: Expr,
    ) -> Expr {
        e(ExprKind::Lambda(Lambda {
            captures: captures.iter().map(|c| id(c)).collect(),
            params: params.into_iter().map(|(pat, ty)| LambdaParam { pat, ty }).collect(),
            ret,
            body: Box::new(body),
        }))
    }

    pub fn bag_lit(items: Vec<Expr>) -> Expr {
        e(ExprKind::BagLiteral(items))
    }

    pub fn map_lit(entries: Vec<(Expr, Expr)>) -> Expr {
        e(ExprKind::MapLit(entries))
    }

    pub fn record_upd(base: Expr, fields: Vec<(&str, Expr)>) -> Expr {
        e(ExprKind::RecordUpd {
            base: Box::new(base),
            fields: fields.into_iter().map(|(n, v)| FieldInit { name: id(n), value: v }).collect(),
        })
    }

    pub fn str_interp(parts: Vec<StrPart>) -> Expr {
        e(ExprKind::StrInterp(parts))
    }
}

pub mod decl {
    use super::*;

    pub fn param(name: &str, ty: Type) -> Param {
        Param { name: id(name), ty }
    }

    pub fn operator(
        level: EffectLevel,
        recursive: bool,
        name: &str,
        params: Vec<Param>,
        ret: Type,
        body: Expr,
    ) -> OperatorDecl {
        OperatorDecl {
            vis: Visibility::Private,
            level,
            recursive,
            name: id(name),
            type_params: vec![],
            params,
            ret,
            decreases: None,
            depth: None,
            body: Some(body),
        }
    }

    pub fn function(name: &str, params: Vec<Param>, ret: Type, body: Expr) -> Item {
        Item::Operator(operator(EffectLevel::Function, false, name, params, ret, body))
    }

    pub fn function_rec(name: &str, params: Vec<Param>, ret: Type, body: Expr) -> Item {
        Item::Operator(operator(EffectLevel::Function, true, name, params, ret, body))
    }

    /// A generic pure function (type parameters, body present).
    pub fn function_gen(
        name: &str,
        tparams: &[&str],
        params: Vec<Param>,
        ret: Type,
        body: Expr,
    ) -> Item {
        let mut op = operator(EffectLevel::Function, false, name, params, ret, body);
        op.type_params = tparams.iter().map(|t| id(t)).collect();
        Item::Operator(op)
    }

    /// An external pure function (no body).
    pub fn function_ext(name: &str, tparams: &[&str], params: Vec<Param>, ret: Type) -> Item {
        let mut op = operator(EffectLevel::Function, false, name, params, ret, expr::int(0));
        op.type_params = tparams.iter().map(|t| id(t)).collect();
        op.body = None;
        Item::Operator(op)
    }

    pub fn query(name: &str, params: Vec<Param>, ret: Type, body: Expr) -> Item {
        Item::Operator(operator(EffectLevel::Query, false, name, params, ret, body))
    }

    pub fn action(name: &str, params: Vec<Param>, body: Expr) -> Item {
        Item::Operator(operator(
            EffectLevel::Action,
            false,
            name,
            params,
            ty::set(ty::named("write_op")),
            body,
        ))
    }

    pub fn const_(name: &str, ty: Type, value: Expr) -> Item {
        Item::Const(ConstDecl { vis: Visibility::Private, name: id(name), ty, value })
    }

    pub fn variant_unit(name: &str) -> Variant {
        Variant { name: id(name), payload: VariantPayload::None }
    }

    pub fn variant_tuple(name: &str, payload: Vec<Type>) -> Variant {
        Variant { name: id(name), payload: VariantPayload::Tuple(payload) }
    }

    pub fn enum_(name: &str, variants: Vec<Variant>) -> Item {
        Item::Enum(EnumDecl {
            vis: Visibility::Private,
            name: id(name),
            params: vec![],
            variants,
        })
    }

    /// A generic enum declaration.
    pub fn enum_gen(name: &str, tparams: &[&str], variants: Vec<Variant>) -> Item {
        Item::Enum(EnumDecl {
            vis: Visibility::Private,
            name: id(name),
            params: tparams.iter().map(|t| id(t)).collect(),
            variants,
        })
    }

    /// A record-payload variant: `name { f: T, ... }` (§3.2).
    pub fn variant_record(name: &str, fields: Vec<(&str, Type)>) -> Variant {
        Variant {
            name: id(name),
            payload: VariantPayload::Record(fields.into_iter().map(|(n, t)| (id(n), t)).collect()),
        }
    }

    pub fn table(name: &str, fields: Vec<(&str, Type)>, pk: &[&str]) -> Item {
        Item::Table(TableDecl {
            vis: Visibility::Private,
            name: id(name),
            fields: fields.into_iter().map(|(n, t)| (id(n), t)).collect(),
            pk: pk.iter().map(|c| id(c)).collect(),
            fks: vec![],
        })
    }

    pub fn module(name: &str, items: Vec<Item>) -> Module {
        Module { name: id(name), items, span: sp() }
    }
}
