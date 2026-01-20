use crate::cfg::cfg_cond::CfgEdge;
use crate::cfg::cfg_node_kind::{CfgNodeKind, NodeId};
use std::collections::HashMap;

pub struct CFGraph {
    pub nodes: Vec<CfgNodeKind>,
    /// Control Flow Graph (CFG) represented as an adjacency list.
    /// Maps each (source, to) node ID to the CfgEdge.
    pub edges: HashMap<(NodeId, NodeId), CfgEdge>,
}