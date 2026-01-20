use crate::cfg::cf_graph::CFGraph;
use crate::cfg::cfg_node_kind::{CfgNodeKind, NodeId};
use std::collections::HashMap;

/// Finite State Machine (FSM) representation
#[derive(Debug)]
pub struct FSMachine {
    states: HashMap<StateId, String>, // Maps StateId to state name
    transitions: Vec<Transition>,     // List of transitions between states
    start: StateId,                   // Starting state ID
    terminals: Vec<StateId>,          // Terminal/accepting state IDs
}

impl FSMachine {
    /// Returns reference to the state mapping
    pub fn state_map(&self) -> &HashMap<StateId, String> {
        &self.states
    }

    /// Returns reference to the transition list
    pub fn transitions(&self) -> &Vec<Transition> {
        &self.transitions
    }

    /// Returns the starting state ID
    pub fn start_id(&self) -> StateId {
        self.start.clone()
    }

    /// Returns reference to terminal states
    pub fn terminals(&self) -> &Vec<StateId> {
        &self.terminals
    }
}

/// Converts Control Flow Graph (CFG) to Finite State Machine (FSM)
pub fn cfg_to_fsm(cfg: &CFGraph) -> FSMachine {
    // Maps CFG node IDs to FSM state IDs
    let mut state_map: HashMap<NodeId, StateId> = HashMap::new();
    let mut states = HashMap::new();      // FSM states
    let mut transitions = Vec::new();     // FSM transitions
    let mut terminals = Vec::new();       // Terminal states

    let mut next_state_id = 0;            // Counter for generating unique state IDs

    // Helper closure to create new FSM state
    let mut new_state = |name: String| {
        let id = StateId(next_state_id);
        next_state_id += 1;
        states.insert(id.clone(), name);
        id
    };

    // Step 1: Create FSM states for Action/Start/End nodes
    for (i, node) in cfg.nodes.iter().enumerate() {
        let node_id = NodeId(i);

        match node {
            CfgNodeKind::Start => {
                let sid = new_state("START".into());
                state_map.insert(node_id, sid);
            }

            CfgNodeKind::Action(name) => {
                let sid = new_state(name.clone());
                state_map.insert(node_id, sid);
            }

            CfgNodeKind::End => {
                let sid = new_state("END".into());
                terminals.push(sid.clone());  // End nodes are terminal states
                state_map.insert(node_id, sid);
            }

            CfgNodeKind::Decision(_) => {
                // Decision nodes are NOT FSM states (handled in transitions)
            }
            CfgNodeKind::Merge => {}
            CfgNodeKind::Stop => {}
        }
    }

    // Find the START state
    let start = state_map
        .iter()
        .find(|(_, s)| states[s] == "START")
        .map(|(_, s)| s)
        .expect("No START state found")
        .clone();

    // Step 2: Resolve transitions between states
    for ((from, to), edge) in cfg.edges.iter() {
        let from_node = &cfg.nodes[from.0];
        let to_node = &cfg.nodes[to.0];

        match (from_node, to_node) {
            // Direct transitions between Action states or Start/End
            (CfgNodeKind::Action(_), CfgNodeKind::Action(_))
            | (CfgNodeKind::Start, CfgNodeKind::Action(_))
            | (CfgNodeKind::Action(_), CfgNodeKind::End)
            | (CfgNodeKind::Start, CfgNodeKind::End) => {
                transitions.push(Transition::new(
                    state_map[&from].clone(),
                    state_map[&to].clone(),
                    edge.condition(),
                ));
            }

            // Decision node resolving to Action
            (CfgNodeKind::Decision(_), CfgNodeKind::Action(_)) => {
                // Find all predecessors of this decision node
                for (_, pred) in cfg.edges.iter()
                    .filter(|(_, e)| e.to == edge.from) {
                    if let Some(from_state) = state_map.get(&pred.from) {
                        // Create transition from predecessor to target action
                        transitions.push(Transition::new(
                            from_state.clone(),
                            state_map[&edge.to].clone(),
                            edge.condition(),
                        ));
                    }
                }
            }

            // Action to Decision (delayed resolution)
            (CfgNodeKind::Action(_), CfgNodeKind::Decision(_)) => {
                // Handled when the decision node resolves to an action
            }

            _ => {} // Other cases not relevant for FSM
        }
    }

    FSMachine {
        states,
        transitions,
        start,
        terminals,
    }
}

use crate::fsm::state_id::StateId;
use crate::fsm::transition::Transition;
use std::fmt::Write;

/// Converts FSM to Graphviz DOT format for visualization
pub fn fsm_to_dot(fsm: &FSMachine) -> String {
    let mut out = String::new();

    writeln!(&mut out, "digraph FSM {{").unwrap();
    writeln!(&mut out, "  rankdir=LR;").unwrap(); // Left-to-right layout
    writeln!(&mut out).unwrap();

    // -------------------------
    // Node definitions
    // -------------------------
    for (id, name) in &fsm.states {
        // Start and terminal states get double circles
        let shape = if *id == fsm.start_id() || fsm.terminals().contains(id) {
            "doublecircle"
        } else {
            "circle"
        };

        writeln!(
            &mut out,
            "  S{} [label=\"{}\", shape={}];",
            id.0,
            escape(name),
            shape
        )
            .unwrap();
    }

    writeln!(&mut out).unwrap();

    // -------------------------
    // Transitions
    // -------------------------
    for t in &fsm.transitions {
        match t.condition() {
            Some(cond) => {
                // Transition with condition label
                writeln!(
                    &mut out,
                    "  S{} -> S{} [label=\"{}\"];",
                    t.from().0,
                    t.to().0,
                    escape(cond)
                )
                    .unwrap();
            }
            None => {
                // Unconditional transition
                writeln!(&mut out, "  S{} -> S{};", t.from().0, t.to().0).unwrap();
            }
        }
    }

    writeln!(&mut out, "}}").unwrap();
    out
}

/// Escapes special characters for DOT format
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")  // Escape backslashes
        .replace('"', "\\\"") // Escape quotes
        .replace('\n', "\\n") // Escape newlines
}