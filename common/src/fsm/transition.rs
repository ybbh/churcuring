use crate::fsm::state_id::StateId;

#[derive(Debug)]
pub struct Transition {
    from: StateId,
    to: StateId,
    condition: Option<String>,
}

impl Transition {
    pub fn new(from: StateId, to: StateId, condition: Option<String>) -> Transition {
        Transition { from, to, condition }
    }

    pub fn from(&self) -> StateId {
        self.from
    }

    pub fn to(&self) -> StateId {
        self.to
    }

    pub fn condition(&self) -> &Option<String> {
        &self.condition
    }
}