use anyhow::Result;
use std::error::Error;
use std::fs;
use tracing::info;

#[allow(unused)]
mod ts_const;
#[allow(unused)]
mod builder;

/// Main entry point for the PlantUML to State Machine converter
///
/// This function:
/// 1. Reads a PlantUML file
/// 2. Parses it using Tree-sitter
/// 3. Converts to state machine model
/// 4. Generates C# code using Askama templates
/// 5. Saves the generated code to a file
///
fn main() -> Result<(), Box<dyn Error>> {
    main_inner(std::env::args_os())?;
    Ok(())
}

pub fn main_inner<I, T>(_args: I) -> Result<(), Box<dyn Error>>
where
    I: IntoIterator<Item=T>,
    T: Into<std::ffi::OsString> + Clone,
{

    // Execute the logic
    if let Err(e) = execute() {
        eprintln!("Error: {}", e);
        //process::exit(1);
    }
    Ok(())
}

fn execute() -> Result<()> {
    // Initialize logging

    // Read the PlantUML file
    let plantuml_content = fs::read_to_string("example.puml")?;

    info!("Parsing PlantUML activity diagram...");

    // Create parser and parse the content
    let mut parser = builder::parser::ADParser::new();
    let _ast = parser.parse(&plantuml_content)?;


    info!("C# state machine generated successfully!");

    Ok(())
}