use crate::cfg::cfg_node_kind::NodeId;

/// Represents a directed edge in the Control Flow Graph (CFG).
#[derive(Debug)]
pub struct CfgEdge {
    /// Source node identifier
    pub from: NodeId,
    /// Destination node identifier
    pub to: NodeId,
    /// Optional condition expression for conditional branches
    /// (e.g., "x > 0" for if-statements)
    pub cond: Option<CfgCond>,
    /// Optional edge label for additional metadata
    /// (e.g., "loop_back_edge", "exception_handler")
    pub label: Option<String>,

}

#[derive(Debug)]
pub struct CfgCond {
    seq: u64,
    name: String,
}

impl CfgCond {
    pub fn new(seq: u64, name: String) -> CfgCond {
        Self { seq, name }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn name(&self) -> &String {
        &self.name
    }

    pub fn to_string(&self) -> String {
        format!(" {} {}", self.seq, self.name)
    }
}

