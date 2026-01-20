use crate::cfg::cf_graph::CFGraph;
use crate::cfg::cfg_cond::CfgEdge;
use crate::cfg::cfg_node_kind::{CfgNodeKind, NodeId};
use std::collections::HashMap;

pub struct CfgBuilder {
    pub cfg: CFGraph,

    /// label name -> node where the label points to
    pub labels: HashMap<String, NodeId>,

    /// list of (from_node, label_name) for unresolved gotos
    pub pending_gotos: Vec<(NodeId, String)>,

    /// stack of loop-exit nodes, used for `break`
    pub loop_exit_stack: Vec<NodeId>,
}

impl CfgBuilder {
    pub fn new() -> (Self, NodeId) {
        let mut cfg = CFGraph {
            nodes: Vec::new(),
            edges: HashMap::new(),
        };

        let start = NodeId(cfg.nodes.len());
        cfg.nodes.push(CfgNodeKind::Start);

        (
            Self {
                cfg,
                labels: HashMap::new(),
                pending_gotos: Vec::new(),
                loop_exit_stack: Vec::new(),
            },
            start,
        )
    }

    pub fn new_node(&mut self, kind: CfgNodeKind) -> NodeId {
        let id = NodeId(self.cfg.nodes.len());
        self.cfg.nodes.push(kind);
        id
    }

    /// Inserts a CFG edge into the adjacency list
    pub fn edge(&mut self, edge: CfgEdge) {
        // Add the edge to the edge set
        // Duplicate edges (same from/to pair) will be updated
        let opt = self.cfg.edges.get_mut(&(edge.from, edge.to));
        if let Some(e) = opt {
            if e.cond.is_none() && edge.cond.is_some() {
                e.cond = edge.cond
            }
            if e.label.is_none() && edge.label.is_some() {
                e.label = edge.label
            }
        } else {
            self.cfg.edges.insert((edge.from, edge.to), edge);
        }
    }
}
