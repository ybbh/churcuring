#[cfg(test)]
mod tests {
    use crate::builder::builder::build_fsm_from_plantuml;
    use crate::builder::csharp::csharp_gen::generate_csharp_fsm;
    use common::fsm::fs_machine::fsm_to_dot;


    #[test]
    fn test_builder() {
        for text in [
            /*
            include_str!("../test_data/break-in-while.puml" ),
            include_str!("../test_data/goto-label.puml"),
            include_str!("../test_data/goto-loop.puml"),
            include_str!("../test_data/if-else.puml"),
            */
            include_str!("../test_data/if-elseif-else.puml"),
            /*
            include_str!("../test_data/repeat-while.puml"),
            include_str!("../test_data/simple.puml"),
            include_str!("../test_data/stop.puml"),
            include_str!("../test_data/while.puml"),
             */
        ] {
            let fsm = build_fsm_from_plantuml(text).unwrap();
            let dot = fsm_to_dot(&fsm);
            println!("{}", &dot);
            let _r = generate_csharp_fsm(&fsm, "./".to_string());
        }
    }
}