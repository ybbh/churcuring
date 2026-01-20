#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug)]
pub enum CfgNodeKind {
    Start,
    Action(String),
    Decision(String),
    Merge,
    Stop,
    End,
}
