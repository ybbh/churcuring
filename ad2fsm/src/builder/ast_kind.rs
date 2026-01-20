#[derive(Clone, Debug)]
pub struct CondBody {
    pub cond: String,
    pub body: Vec<ASTKind>,
}

#[derive(Clone, Debug)]
pub struct IfElseBlock {
    pub if_elif: Vec<CondBody>,
    pub else_: Vec<ASTKind>,
}

#[derive(Clone, Debug)]
pub enum ASTKind {
    SimpleStmt(String),
    ActivityRef(String),
    While(CondBody),
    Case(Vec<CondBody>),
    IfElse(IfElseBlock),
    Label(String),
    Goto(String),
    Break,
    Stop,
}