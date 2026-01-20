use crate::builder::lower::build_cfg;
use crate::builder::parser::parse_with_tree_sitter;
use common::fsm::fs_machine::{cfg_to_fsm, FSMachine};

pub fn build_fsm_from_plantuml(text: &str) -> anyhow::Result<FSMachine> {
    let ast = parse_with_tree_sitter(text)?;

    let cfg = build_cfg(&ast)?;

    let fsm = cfg_to_fsm(&cfg);

    Ok(fsm)
}

#[cfg(test)]
mod tests {
    use super::build_fsm_from_plantuml;
    use common::fsm::fs_machine::fsm_to_dot;

    #[test]
    fn test_builder() {
        for text in [
            include_str!("test_data/break-in-while.puml"),
            include_str!("test_data/goto-label.puml"),
            include_str!("test_data/goto-loop.puml"),
            include_str!("test_data/if-else.puml"),
            include_str!("test_data/if-elseif-else.puml"),
            include_str!("test_data/repeat-while.puml"),
            include_str!("test_data/simple.puml"),
            include_str!("test_data/stop.puml"),
            include_str!("test_data/while.puml"),
        ] {
            let fsm = build_fsm_from_plantuml(text).unwrap();
            let dot = fsm_to_dot(&fsm);
            println!("{}", dot);
        }
    }
}

