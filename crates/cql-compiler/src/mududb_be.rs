//! mududb backend — placeholder (doc/backend-mududb.md, doc/codegen-backend.md §4).
//!
//! The `mududb_syscall_v1` contract is a **proposal**: exact syscall
//! signatures and numbers are TBD pending `mududb_p/doc/lang.common`
//! (backend-mududb.md §3/§9). This backend therefore does NOT emit wasm
//! components and does NOT hardcode any syscall numbers. Instead it emits a
//! **deployment plan** (text) per module:
//!
//! - a component interface skeleton (backend-mududb.md §7.1): table imports
//!   with §6.2 ABI types, the `mududb_syscall_v1` host import placeholder,
//!   and one export per public query/action;
//! - a per-operator syscall *call-sequence skeleton* derived from the CIR:
//!   `Read{plan}` → `tbl_get` / `tbl_scan` (index or full),
//!   `WriteOp` → `cmd_insert` / `cmd_update` / `cmd_delete` inside a txn —
//!   wrapped in session/snapshot open-close, mirroring §7.2/§7.4.
//!
//! All syscall names emitted here are placeholders marked `(proposal)`.
//! Once `lang.common` is available, this backend is expected to grow a real
//! wasm32-wasip2 component emitter next to (or replacing) the plan emitter;
//! the `Backend` trait boundary already isolates that change.

use crate::ast::EffectLevel;
use crate::cir::{CirExpr, CirExprKind, CirModule, CirType, CirWriteOp};
use crate::codegen::{Backend, EmitCtx};
use crate::diag::DiagBag;
use crate::optimize::ReadPlan;

/// The mududb backend placeholder: CIR → deployment plan text.
///
/// The plan is a human-/tool-readable contract draft for the future wasm
/// component build (`cqlc build --backend mududb` writes it as
/// `<module>.mududb-plan.txt`).
#[derive(Debug, Default, Clone, Copy)]
pub struct MududbBackend;

impl Backend for MududbBackend {
    type Output = String;

    fn name(&self) -> &'static str {
        "mududb"
    }

    fn emit(&self, cir: &CirModule, ctx: &EmitCtx) -> Result<String, DiagBag> {
        let mut out = String::new();
        out.push_str(&format!(
            "// mududb deployment plan for module `{}` — PROPOSAL\n\
             // (doc/backend-mududb.md §3/§9)\n\
             // The mududb_syscall_v1 contract is a proposal: no syscall\n\
             // signatures/numbers are hardcoded here. Syscall names below are\n\
             // placeholders from backend-mududb.md §7.2/§7.4 pending alignment\n\
             // with mududb_p/doc/lang.common. ctx.module_name = `{}`.\n\n",
            cir.name, ctx.module_name
        ));

        // ---- component interface skeleton (§7.1) ----
        out.push_str(&format!("component {} {{\n", cir.name));
        for t in &cir.tables {
            let key_tys: Vec<String> = t
                .fields
                .iter()
                .filter(|(n, _)| t.pk.contains(n))
                .map(|(_, ty)| abi_ty(ty))
                .collect();
            let val_fields: Vec<String> = t
                .fields
                .iter()
                .filter(|(n, _)| !t.pk.contains(n))
                .map(|(n, ty)| format!("{n}: {}", abi_ty(ty)))
                .collect();
            out.push_str(&format!(
                "    import {}: table<({}, record {{ {} }})>\n",
                t.name,
                key_tys.join(", "),
                val_fields.join(", ")
            ));
        }
        out.push_str("    import syscalls: mududb_syscall_v1   // host import name TBD (proposal)\n\n");
        for op in &cir.operators {
            let params: Vec<String> = op
                .params
                .iter()
                .map(|(n, ty)| format!("{n}: {}", abi_ty(ty)))
                .collect();
            out.push_str(&format!(
                "    export {}: ({}) -> {}\n",
                op.name,
                params.join(", "),
                abi_ty(&op.ret)
            ));
        }
        out.push_str("}\n");

        // ---- per-operator call-sequence skeleton (§7.2/§7.4) ----
        for op in &cir.operators {
            let mut ops = Vec::new();
            collect_plan_ops(&op.body, &mut ops);
            let reads: Vec<&PlanOp> = ops
                .iter()
                .filter(|o| matches!(o, PlanOp::Read { .. }))
                .collect();
            let writes: Vec<&PlanOp> = ops
                .iter()
                .filter(|o| matches!(o, PlanOp::Write { .. }))
                .collect();
            let read_tables: Vec<String> = {
                let mut ts: Vec<String> = reads
                    .iter()
                    .map(|o| match o {
                        PlanOp::Read { table, .. } => table.clone(),
                        _ => unreachable!(),
                    })
                    .collect();
                ts.sort();
                ts.dedup();
                ts
            };

            let kind = match op.level {
                EffectLevel::Function => "function",
                EffectLevel::Query => "query",
                EffectLevel::Action => "action",
            };
            out.push_str(&format!("\nprocedure {} ({kind}):\n", op.name));
            out.push_str("    sess := session_open()?                       // (1) session (proposal)\n");
            if matches!(op.level, EffectLevel::Action) && !writes.is_empty() {
                out.push_str("    txn := txn_begin(sess)                          // action writes (proposal)\n");
            }
            if !read_tables.is_empty() {
                out.push_str(&format!(
                    "    snap := snapshot_begin(sess, [{}])? // (2) snapshot reads (proposal)\n",
                    read_tables.join(", ")
                ));
            }
            for o in &ops {
                match o {
                    PlanOp::Read { table, plan } => {
                        let call = match plan.as_str() {
                            s if s.starts_with("point-lookup") => {
                                format!("tbl_get(snap, {table}, key)?")
                            }
                            s if s.starts_with("index-scan") => {
                                format!("tbl_scan(snap, {table}, index={})?", &s["index-scan".len()..])
                            }
                            _ => format!("tbl_scan(snap, {table}, full)?"),
                        };
                        out.push_str(&format!("    -- read {table} [plan: {plan}]: {call}  (proposal)\n"));
                    }
                    PlanOp::Write { kind, table } => {
                        out.push_str(&format!("    cmd_{kind}(txn, {table}, ...)        // (3) write (proposal)\n"));
                    }
                }
            }
            if matches!(op.level, EffectLevel::Action) && !writes.is_empty() {
                out.push_str(
                    "    txn_commit(txn)   // kernel enforces FK/invariants (§6 responsibility split, proposal)\n",
                );
            }
            out.push_str("    session_close(sess)\n");
        }

        // ---- invariants / FK note ----
        if !cir.invariants.is_empty() {
            out.push_str("\n// invariants enforced by the kernel at txn_commit (proposal; if the kernel\n");
            out.push_str("// does not support this, fall back to component-side pre-write self-check\n");
            out.push_str("// reads, see backend-mududb.md §9):\n");
            for inv in &cir.invariants {
                out.push_str(&format!("//   invariant {} on {}\n", inv.name, inv.table));
            }
        }
        out.push_str(
            "\n// TBD (backend-mududb.md §9): syscall signatures/numbers, transaction SQL,\n\
             // host import shape, adapter behavior differences. This file is not a\n\
             // stable contract.\n",
        );
        Ok(out)
    }
}

/// One table-access step in an operator's call sequence.
#[derive(Debug, Clone, PartialEq)]
enum PlanOp {
    Read { table: String, plan: String },
    Write { kind: &'static str, table: String },
}

/// Pre-order walk collecting table reads/writes in evaluation order.
fn collect_plan_ops(e: &CirExpr, out: &mut Vec<PlanOp>) {
    match &e.kind {
        CirExprKind::Read { table, plan, .. } => {
            let plan = match plan {
                ReadPlan::PointLookup => "point-lookup".to_string(),
                ReadPlan::IndexScan { index } => format!("index-scan({})", index.node),
                ReadPlan::FullScan => "full-scan".to_string(),
            };
            out.push(PlanOp::Read {
                table: table.clone(),
                plan,
            });
        }
        CirExprKind::WriteOp(w) => {
            let (kind, table) = match w {
                CirWriteOp::Insert { table, .. } => ("insert", table.clone()),
                CirWriteOp::Update { table, .. } => ("update", table.clone()),
                CirWriteOp::Delete { table, .. } => ("delete", table.clone()),
            };
            out.push(PlanOp::Write { kind, table });
        }
        _ => {}
    }
    for child in children(e) {
        collect_plan_ops(child, out);
    }
}

/// Shallow child walk over all CIR expression variants.
fn children(e: &CirExpr) -> Vec<&CirExpr> {
    match &e.kind {
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
        CirExprKind::Field { base, .. } | CirExprKind::TupleProj { base, .. } => vec![base.as_ref()],
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
        CirExprKind::Lit(_)
        | CirExprKind::Var(_)
        | CirExprKind::EnvGet(_)
        | CirExprKind::ConstRef(_)
        | CirExprKind::FunRef { .. }
        | CirExprKind::StdLibRef { .. }
        | CirExprKind::OptionNone => vec![],
    }
}

/// §6.2 ABI vocabulary rendering for the component skeleton.
fn abi_ty(ty: &CirType) -> String {
    match ty {
        CirType::Bool => "bool".into(),
        CirType::Int => "s64".into(),
        CirType::Float => "f64".into(),
        CirType::Decimal(_) => "decimal".into(),
        CirType::String => "string".into(),
        CirType::Date => "date".into(),
        CirType::Option(t) => format!("option<{}>", abi_ty(t)),
        CirType::Vector(t) => format!("list<{}>", abi_ty(t)),
        CirType::Set(t) => format!("set<{}>", abi_ty(t)),
        CirType::Bag(t) => format!("bag<{}>", abi_ty(t)),
        CirType::Map(k, v) => format!("map<{}, {}>", abi_ty(k), abi_ty(v)),
        CirType::Tuple(ts) => format!("({})", ts.iter().map(abi_ty).collect::<Vec<_>>().join(", ")),
        CirType::Record(n) | CirType::Row(n) | CirType::Enum(n) => n.clone(),
        CirType::Fun(a, b) => format!("func({}) -> {}", abi_ty(a), abi_ty(b)),
        CirType::WriteOp => "write-op".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::compile_module;

    fn plan_for(src: &str) -> String {
        let (opt, bag) = compile_module(src);
        assert!(!bag.has_errors(), "compile errors: {bag:?}");
        let cir = crate::cir::lower_to_cir(&opt.expect("module")).expect("cir");
        MududbBackend
            .emit(&cir, &EmitCtx::new("m"))
            .expect("mududb plan")
    }

    #[test]
    fn plan_contains_component_skeleton_and_no_syscall_numbers() {
        let src = "module m;
table users { id: int, name: string } primary key {id}
query q() -> int == { 1 }
";
        let plan = plan_for(src);
        assert!(plan.contains("component m {"));
        assert!(plan.contains("import users: table<(s64, record { name: string })>"));
        assert!(plan.contains("mududb_syscall_v1"));
        assert!(plan.contains("export q: () -> s64"));
        assert!(plan.contains("PROPOSAL"));
        // No hardcoded syscall numbers (e.g. `syscall 3` / `nr = 7`).
        assert!(!plan.contains("syscall 0") && !plan.contains("nr ="));
    }

    #[test]
    fn read_plans_map_to_syscall_skeletons() {
        let src = "module m;
table users { id: int, name: string } primary key {id}
query by_id(i: int) -> option<{ id: int, name: string }> == {
    lookup(users, i)
}
query count_all() -> int == {
    fold(to_vector(users), 0, lambda(acc, u) { acc + 1 })
}
";
        let plan = plan_for(src);
        assert!(plan.contains("[plan: point-lookup]: tbl_get(snap, users, key)?"), "{plan}");
        assert!(plan.contains("[plan: full-scan]: tbl_scan(snap, users, full)?"), "{plan}");
        assert!(plan.contains("snapshot_begin(sess, [users])"), "{plan}");
    }

    #[test]
    fn action_writes_map_to_cmd_sequence_with_txn() {
        let src = "module m;
table accounts { id: int, balance: int } primary key {id}
action deposit(to: int, amount: int) -> set<write_op> == {
    set { update(accounts, to, lambda [amount](v) { record { v with balance: v.balance + amount } }) }
}
";
        let plan = plan_for(src);
        assert!(plan.contains("txn := txn_begin(sess)"), "{plan}");
        assert!(plan.contains("cmd_update(txn, accounts, ...)"), "{plan}");
        assert!(plan.contains("txn_commit(txn)"), "{plan}");
    }
}
