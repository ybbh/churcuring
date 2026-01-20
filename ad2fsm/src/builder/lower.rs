use crate::builder::ast_kind::{ASTKind, CondBody, IfElseBlock};
use anyhow::{anyhow, Result};
use common::cfg::cf_graph::CFGraph;
use common::cfg::cfg_builder::CfgBuilder;
use common::cfg::cfg_cond::{CfgCond, CfgEdge};
use common::cfg::cfg_node_kind::{CfgNodeKind, NodeId};

pub fn lower_block(
    builder: &mut CfgBuilder,
    block: &[ASTKind],
    entry: NodeId,
) -> NodeId {
    let mut current = entry;

    for stmt in block {
        current = lower_stmt(builder, stmt, current);
    }

    current
}

pub fn lower_stmt(
    builder: &mut CfgBuilder,
    stmt: &ASTKind,
    entry: NodeId,
) -> NodeId {
    match stmt {
        ASTKind::SimpleStmt(text) => {
            let n = builder.new_node(CfgNodeKind::Action(text.clone()));
            builder.edge(CfgEdge::new(entry, n, None, None));
            n
        }

        ASTKind::ActivityRef(name) => {
            let n = builder.new_node(CfgNodeKind::Action(format!("call {}", name)));
            builder.edge(CfgEdge::new(entry, n, None, None));
            n
        }

        // ---------------------------
        // LABEL (no node created)
        // ---------------------------
        ASTKind::Label(name) => {
            // Label points to the current entry node
            builder.labels.insert(name.clone(), entry);
            entry
        }

        // ---------------------------
        // GOTO (unconditional jump)
        // ---------------------------
        ASTKind::Goto(label) => {
            let n = builder.new_node(CfgNodeKind::Action(format!("goto {}", label)));
            builder.edge(CfgEdge::new(entry, n, None, None));

            // resolve later
            builder.pending_gotos.push((n, label.clone()));
            n
        }

        // ---------------------------
        // BREAK (jump to loop exit)
        // ---------------------------
        ASTKind::Break => {
            let Some(&exit) = builder.loop_exit_stack.last()
            else { panic!("break outside loop"); };

            let n = builder.new_node(CfgNodeKind::Action("break".into()));
            builder.edge(CfgEdge::new(entry, n, None, None));
            builder.edge(CfgEdge::new(n, exit, None, None));
            n
        }

        // ---------------------------
        // STOP (terminal node)
        // ---------------------------
        ASTKind::Stop => {
            let n = builder.new_node(CfgNodeKind::End);
            builder.edge(CfgEdge::new(entry, n, None, None));
            n
        }

        // ---------------------------
        // IF / ELSE
        // ---------------------------
        ASTKind::IfElse(block) => {
            lower_ifelse(builder, block, entry)
        }

        // ---------------------------
        // WHILE LOOP
        // ---------------------------
        ASTKind::While(cond) => {
            lower_while(builder, cond, entry)
        }

        // ---------------------------
        // CASE (switch-like)
        // ---------------------------
        ASTKind::Case(cases) => {
            lower_case(builder, cases, entry)
        }
    }
}


fn lower_ifelse(
    builder: &mut CfgBuilder,
    block: &IfElseBlock,
    entry: NodeId,
) -> NodeId {
    let merge = builder.new_node(CfgNodeKind::Action("if_end".into()));
    let decision = builder.new_node(CfgNodeKind::Decision("if".into()));
    builder.edge(CfgEdge::new(entry, decision, None, None));

    for (i, cond) in block.if_elif.iter().enumerate() {
        let body_entry = lower_block(builder, &cond.body, decision);
        let cfg_cond = CfgCond::new(
            i as _,
            cond.cond.clone(),
        );
        builder.edge(CfgEdge::new(decision, body_entry, Some(cond.cond.clone()), Some(cfg_cond)));
        builder.edge(CfgEdge::new(body_entry, merge, None, None));
    }

    if !block.else_.is_empty() {
        let else_entry = lower_block(builder, &block.else_, decision);
        let cfg_cond = CfgCond::new(
            (block.if_elif.len() + 1usize) as _,
            "else".to_string(),
        );
        builder.edge(CfgEdge::new(decision, else_entry, None, Some(cfg_cond)));
        builder.edge(CfgEdge::new(else_entry, merge, None, None));
    }

    merge
}


fn lower_while(
    builder: &mut CfgBuilder,
    cond: &CondBody,
    entry: NodeId,
) -> NodeId {
    let decision = builder.new_node(CfgNodeKind::Decision(cond.cond.clone()));
    let merge = builder.new_node(CfgNodeKind::Action("while_end".into()));

    builder.edge(CfgEdge::new(entry, decision, None, None));
    builder.loop_exit_stack.push(merge);

    let body_exit = lower_block(builder, &cond.body, decision);
    let cfg_cond_true = CfgCond::new(
        0 as _,
        "true".to_string(),
    );
    let cfg_cond_false = CfgCond::new(
        1 as _,
        "false".to_string(),
    );
    builder.edge(CfgEdge::new(decision, body_exit, None, Some(cfg_cond_true)));
    builder.edge(CfgEdge::new(body_exit, decision, None, None));
    builder.edge(CfgEdge::new(decision, merge, None, Some(cfg_cond_false)));

    builder.loop_exit_stack.pop();
    merge
}


fn lower_case(
    builder: &mut CfgBuilder,
    cases: &[CondBody],
    entry: NodeId,
) -> NodeId {
    let decision = builder.new_node(CfgNodeKind::Decision("case".into()));
    let merge = builder.new_node(CfgNodeKind::Action("case_end".into()));
    builder.edge(CfgEdge::new(entry, decision, None, None));

    for (i, c) in cases.iter().enumerate() {
        let body_exit = lower_block(builder, &c.body, decision);
        let cfg_cond = CfgCond::new(
            i as _,
            c.cond.clone(),
        );
        builder.edge(CfgEdge::new(decision, body_exit, None, Some(cfg_cond)));
        builder.edge(CfgEdge::new(body_exit, merge, None, None));
    }

    merge
}

pub fn resolve_gotos_step2(builder: &mut CfgBuilder) -> Result<()> {
    for (from, label) in builder.pending_gotos.clone() {
        if let Some(&target) = builder.labels.get(&label) {
            builder.edge(CfgEdge::new(from, target, None, None));
        } else {
            return Err(anyhow!(format!("Undefined label: {}", label)));
        }
    }
    Ok(())
}


pub fn build_cfg(ast: &[ASTKind]) -> Result<CFGraph> {
    let (mut builder, start) = CfgBuilder::new();
    let _exit = lower_block(&mut builder, ast, start);
    resolve_gotos_step2(&mut builder)?;
    Ok(builder.cfg)
}