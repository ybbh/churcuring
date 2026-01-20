use askama::Template;
use std::collections::HashMap;

/// Askama templates
#[derive(Template)]
#[template(path = "csharp/states.cs.j2")]
pub struct StateEnumTemplate {
    pub states: Vec<String>,
}


/// FSM dispatcher template
#[derive(Template)]
#[template(path = "csharp/dispatcher.cs.j2")]
pub struct DispatcherTemplate {
    pub states: Vec<String>,

    /// from_state -> list of Transition class names
    pub dispatch_map: HashMap<String, Vec<String>>,
}


/// One transition = one C# file
#[derive(Template)]
#[template(path = "csharp/transition.cs.j2")]
pub struct TransitionTemplate {
    pub t: TransitionView,
}

#[derive(Debug, Clone)]
pub struct TransitionView {
    pub from: String,
    pub to: String,
    pub class_name: String,
    #[allow(unused)]
    pub func_name: String,
    pub comment: String,
    pub condition: Option<String>,
}


