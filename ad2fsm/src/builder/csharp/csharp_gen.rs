use crate::builder::csharp::template::{DispatcherTemplate, StateEnumTemplate, TransitionTemplate, TransitionView};
use anyhow::Result;
use askama::Template;
use common::fsm::fs_machine::FSMachine;
use common::fsm::state_id::StateId;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
// Entry point: generate full C# FSM

pub fn generate_csharp_fsm<P: AsRef<Path>>(fsm: &FSMachine, out_dir: P) -> Result<()> {
    generate_csharp_fsm_bundle(fsm, out_dir.as_ref())
}

fn sanitize_enum_name(s:&str, id:&StateId) -> String {
    sanitize_fn_name(&format!("{}_{}", s, id.0))
}

/// Generate a full C# FSM bundle:
/// - State.cs
/// - Dispatcher.cs
/// - Transition_X_Y.cs (one file per transition)
pub fn generate_csharp_fsm_bundle(
    fsm: &FSMachine,
    out_dir: &Path,
) -> Result<()> {

    // --------------------------------------------------
    // 1. Collect and sort states for stable generation
    // --------------------------------------------------

    let mut map = HashMap::new();
    for (id, name) in fsm.state_map().iter() {
        map.insert(name.clone(), id.clone());
    }

    let mut ordered_states: Vec<(StateId, String)> =
        fsm.state_map().iter().map(|(id, name)|  (id.clone(), name.clone())).collect();

    ordered_states.sort_by_key(|(id, _)| id.0);

    let state_names: Vec<String> =
        ordered_states.iter().map(|(i, name)| sanitize_enum_name(name, i)).collect();

    // --------------------------------------------------
    // 2. Generate State enum (State.cs)
    // --------------------------------------------------

    let state_enum = StateEnumTemplate {
        states: state_names.clone(),
    };

    fs::write(
        out_dir.join("State.cs"),
        state_enum.render()?,
    )?;

    // --------------------------------------------------
    // 3. Build transition views and dispatch map
    // --------------------------------------------------

    let mut transitions: Vec<TransitionView> = Vec::new();

    // from_state -> list of transition class names
    let mut dispatch_map: HashMap<String, Vec<String>> = HashMap::new();

    for t in fsm.transitions() {
        let from = fsm.state_map()[&t.from()].clone();
        let to = fsm.state_map()[&t.to()].clone();
        let from = sanitize_enum_name(from.as_str(), &t.from());
        let to = sanitize_enum_name(to.as_str(), &t.to());
        let class_name = format!("Transition_{}_{}", from, to);

        let comment = match t.condition() {
            Some(cond) => cond.clone(),
            None => format!("{} -> {}", from, to),
        };

        transitions.push(TransitionView {
            from:from.clone(),
            to:to.clone(),
            class_name: sanitize_fn_name(class_name.as_str()),
            func_name: "".to_string(),
            comment,
            condition: t.condition().as_ref()
                .map(|cond| sanitize_fn_name(cond.as_str())),
        });

        dispatch_map
            .entry(from)
            .or_default()
            .push(class_name);
    }

    // --------------------------------------------------
    // 4. Generate Transition_*.cs files
    // --------------------------------------------------

    for tr in &transitions {
        let tpl = TransitionTemplate {
            t: tr.clone(),
        };

        fs::write(
            out_dir.join(format!("{}.cs", tr.class_name)),
            tpl.render()?,
        )?;
    }

    // --------------------------------------------------
    // 5. Generate Dispatcher.cs
    // --------------------------------------------------


    let dispatcher = DispatcherTemplate {
        states: dispatch_map.keys().cloned().collect(),
        dispatch_map,
    };

    fs::write(
        out_dir.join("Dispatcher.cs"),
        dispatcher.render()?,
    )?;

    Ok(())
}

#[allow(unused)]
fn sanitize_fn_name(s: &str) -> String {
    s.to_lowercase()
        .replace(" ", "_")
        .replace("=", "_eq_")
        .replace(">", "_gt_")
        .replace("<", "_lt_")
        .replace("==", "_eq_")
        .replace("!=", "_ne_")
        .replace("&&", "_and_")
        .replace("||", "_or_")
        .replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
        .trim_matches('_')
        .to_string()
}


