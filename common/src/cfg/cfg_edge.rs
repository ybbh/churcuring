use crate::cfg::cfg_cond::{CfgCond, CfgEdge};
use crate::cfg::cfg_node_kind::NodeId;

impl CfgEdge {
    pub fn new(from: NodeId, to: NodeId, label: Option<String>, cond: Option<CfgCond>) -> CfgEdge {
        Self {
            from,
            to,
            label,
            cond,
        }
    }

    pub fn condition(&self) -> Option<String> {
        let condition = self.label.clone();
        let condition = if let Some(_cond) = condition {
            self.cond.as_ref().map(|cond| { _cond + &cond.to_string() })
        } else {
            self.cond.as_ref().map(|cond| cond.to_string())
        };
        condition
    }
}