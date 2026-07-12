//! cqlc: CQL compiler CLI.
//!
//! Subcommands (doc/todo.md Phase 4, doc/model-check.md §6.2):
//! - `new`    — scaffold a cql.toml + src/main.cql template;
//! - `check`  — run the full pipeline on a project or single file and only
//!   report diagnostics;
//! - `build`  — check + CIR lowering + Rust codegen into out_dir, write a
//!   buildable cargo crate and run `cargo build --offline`;
//! - `test`   — like build, but runs `cargo test --offline` (CQL test blocks);
//! - `clean`  — delete out_dir;
//! - `verify` — model checking: mc_lower lowers to a cql-mc McSpec, the
//!   stateright explicit-state backend gives a per-property verdict
//!   (doc/model-check.md §6.2; exit codes 0/1/2).

mod manifest;
mod project;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use miette::IntoDiagnostic;

/// cqlc: CQL compiler CLI.
#[derive(Parser)]
#[command(name = "cqlc", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold: generate a cql.toml + src/main.cql template.
    New { name: String },
    /// Run the full pipeline on the whole project; only report diagnostics.
    Check {
        /// Project directory or a single .cql file (compiled as a standalone
        /// module when no cql.toml is found).
        path: Option<PathBuf>,
    },
    /// Codegen each module into out_dir in dependency topological order, then
    /// run cargo build.
    Build {
        path: Option<PathBuf>,
        /// Override the backend setting from cql.toml (rust | mududb).
        #[arg(long)]
        backend: Option<String>,
    },
    /// Run cargo test in out_dir after codegen (CQL test blocks → Rust
    /// #[test]).
    Test { path: Option<PathBuf> },
    /// Model checking (doc/model-check.md §6.2).
    Verify {
        path: Option<PathBuf>,
        /// Bounded-layer properties only (invariants/termination/traps).
        #[arg(long)]
        bounded: bool,
        /// Temporal properties only.
        #[arg(long)]
        temporal: bool,
        /// Override the default recursion depth bound.
        #[arg(long)]
        depth: Option<u32>,
        /// Override the default trace length bound.
        #[arg(long)]
        trace: Option<u32>,
        /// Engine: stateright | z3.
        #[arg(long, default_value = "stateright")]
        engine: String,
        /// Replay a counterexample (generates a test block).
        #[arg(long)]
        replay: Option<String>,
    },
    /// Clean out_dir.
    Clean { path: Option<PathBuf> },
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().into_diagnostic()?;
    match cli.command {
        Command::New { name } => cmd_new(&cwd, &name),
        Command::Check { path } => cmd_check(&cwd, path.as_deref()),
        Command::Build { path, backend } => cmd_build(&cwd, path.as_deref(), backend.as_deref()),
        Command::Test { path } => cmd_test(&cwd, path.as_deref()),
        Command::Verify {
            path,
            bounded,
            temporal,
            depth,
            trace,
            engine,
            replay,
        } => cmd_verify(
            &cwd,
            path.as_deref(),
            bounded,
            temporal,
            depth,
            trace,
            &engine,
            replay.as_deref(),
        ),
        Command::Clean { path } => cmd_clean(&cwd, path.as_deref()),
    }
}

fn cmd_new(cwd: &Path, name: &str) -> miette::Result<()> {
    let dir = cwd.join(name);
    if dir.exists() {
        miette::bail!("directory already exists: {}", dir.display());
    }
    std::fs::create_dir_all(dir.join("src")).into_diagnostic()?;
    std::fs::write(
        dir.join("cql.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[build]\nsource_root = \"src\"\nout_dir = \"target/cql\"\nbackend = \"rust\"\n"
        ),
    )
    .into_diagnostic()?;
    std::fs::write(
        dir.join("src").join("main.cql"),
        format!(
            "module {name};\n\nquery hello() -> string == {{\n    \"hello from {name}\"\n}}\n"
        ),
    )
    .into_diagnostic()?;
    println!("created CQL project `{name}` at {}", dir.display());
    Ok(())
}

fn cmd_check(cwd: &Path, path: Option<&Path>) -> miette::Result<()> {
    let target = project::resolve_target(cwd, path).map_err(miette::Report::msg)?;
    let (out, bag) = project::check(&target).map_err(miette::Report::msg)?;
    let has_errors = project::print_diags(&bag);
    if has_errors {
        eprintln!(
            "error: could not compile `{}` due to {} error(s)",
            target.display_name(),
            bag.error_count()
        );
        std::process::exit(1);
    }
    let modules = out.map(|o| o.modules.len()).unwrap_or(0);
    println!(
        "check passed: `{}` ({} module(s), {} warning(s))",
        target.display_name(),
        modules,
        bag.warnings().len()
    );
    Ok(())
}

fn cmd_build(cwd: &Path, path: Option<&Path>, backend: Option<&str>) -> miette::Result<()> {
    let target = project::resolve_target(cwd, path).map_err(miette::Report::msg)?;
    let backend = target.backend(backend);
    let out_dir = target.out_dir(cwd);
    if backend == "mududb" {
        // Phase 6: placeholder deployment plan (proposal stage, no syscall
        // numbers; the wasm component build waits on contract alignment with
        // mududb_p/doc/lang.common, see doc/backend-mududb.md §9).
        match project::emit_mududb_plan(&target, &out_dir) {
            Ok(modules) => {
                println!(
                    "generated mududb deployment plan (PROPOSAL) for `{}` ({} module(s): {}) at {}",
                    target.display_name(),
                    modules.len(),
                    modules.join(", "),
                    out_dir.display()
                );
                println!(
                    "note: syscall contract is a proposal — no component build yet (doc/backend-mududb.md §9)"
                );
                return Ok(());
            }
            Err(msg) => {
                eprintln!("error: {msg}");
                std::process::exit(1);
            }
        }
    }
    if backend != "rust" {
        miette::bail!("unknown backend `{backend}` (expected `rust` or `mududb`)");
    }
    match project::emit_crate(&target, &out_dir) {
        Ok(modules) => {
            println!(
                "generated Rust crate for `{}` ({} module(s): {}) at {}",
                target.display_name(),
                modules.len(),
                modules.join(", "),
                out_dir.display()
            );
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    }
    match project::run_cargo(&out_dir, "build") {
        Ok(true) => {
            println!("cargo build succeeded ({})", out_dir.display());
            Ok(())
        }
        Ok(false) => {
            eprintln!("error: `cargo build --offline` failed in {}", out_dir.display());
            std::process::exit(1);
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    }
}

fn cmd_test(cwd: &Path, path: Option<&Path>) -> miette::Result<()> {
    let target = project::resolve_target(cwd, path).map_err(miette::Report::msg)?;
    let backend = target.backend(None);
    if backend != "rust" {
        miette::bail!("`cqlc test` requires the `rust` backend (current: `{backend}`)");
    }
    let out_dir = target.out_dir(cwd);
    match project::emit_crate(&target, &out_dir) {
        Ok(modules) => {
            println!(
                "generated Rust crate for `{}` ({} module(s): {}) at {}",
                target.display_name(),
                modules.len(),
                modules.join(", "),
                out_dir.display()
            );
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    }
    match project::run_cargo(&out_dir, "test") {
        Ok(true) => {
            println!("cargo test succeeded ({})", out_dir.display());
            Ok(())
        }
        Ok(false) => {
            eprintln!("error: `cargo test --offline` failed in {}", out_dir.display());
            std::process::exit(1);
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    }
}

/// `verify.toml` (+ CLI overrides) → `cql_compiler::mc_lower::McConfig`.
fn build_mc_config(
    vc: &manifest::VerifyConfig,
    depth_override: Option<u32>,
    trace_override: Option<u32>,
) -> Result<cql_compiler::mc_lower::McConfig, String> {
    use cql_compiler::mc_lower::{DomainBound, McConfig};
    let mut domains = std::collections::HashMap::new();
    for (k, v) in &vc.domain {
        // TOML parses `accounts.rows = 2` as a nested table
        // `{accounts = {rows = 2}}`; flatten one level to recover the
        // "accounts.rows" key (doc/model-check.md §6.3).
        if let toml::Value::Table(t) = v {
            for (k2, v2) in t {
                let key = format!("{k}.{k2}");
                domains.insert(key.clone(), DomainBound::from_toml(&key, v2)?);
            }
        } else {
            domains.insert(k.clone(), DomainBound::from_toml(k, v)?);
        }
    }
    let mut config = McConfig {
        depth_default: vc.depth.default,
        depth_per_operator: vc.depth.per_operator.clone(),
        domains,
        trace_length: vc.trace.length,
        fairness_weak: vc.fairness.weak.clone(),
        fairness_strong: vc.fairness.strong.clone(),
    };
    if let Some(d) = depth_override {
        config.depth_default = d;
    }
    if let Some(t) = trace_override {
        config.trace_length = t;
    }
    Ok(config)
}

/// Model checking (doc/model-check.md §6.2).
///
/// Exit codes: 0 = all properties hold within the bounds; 1 = a
/// counterexample was found; 2 = frontend/lowering error.
#[allow(clippy::too_many_arguments)]
fn cmd_verify(
    cwd: &Path,
    path: Option<&Path>,
    bounded: bool,
    temporal: bool,
    depth: Option<u32>,
    trace: Option<u32>,
    engine: &str,
    replay: Option<&str>,
) -> miette::Result<()> {
    if engine != "stateright" {
        miette::bail!(
            "engine `{engine}` is not available in this build (the z3 backend requires the `z3` \
             feature, which needs a prebuilt Z3 — doc/model-check.md §7.3); use `--engine stateright`"
        );
    }
    if let Some(case) = replay {
        miette::bail!("`--replay {case}` is not implemented yet (a Phase 5 follow-up item, doc/model-check.md §8)");
    }
    let target = project::resolve_target(cwd, path).map_err(miette::Report::msg)?;
    let project::Target::Project { root, .. } = &target else {
        miette::bail!("`cqlc verify` requires a CQL project (cql.toml); single-file mode is not supported");
    };

    // verify.toml + CLI overrides.
    let vc = manifest::load_verify_config(root).map_err(miette::Report::msg)?;
    let config = match build_mc_config(&vc, depth, trace) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: invalid verify.toml: {msg}");
            std::process::exit(2);
        }
    };
    if !config.fairness_weak.is_empty() || !config.fairness_strong.is_empty() {
        eprintln!(
            "warning: [fairness] is declared but no backend enforces fairness yet \
             (doc/model-check.md §4.3); all traces are explored"
        );
    }

    // Frontend.
    let sources = project::collect_sources(&target).map_err(miette::Report::msg)?;
    let (out, bag) = cql_compiler::project::compile_project(&sources);
    if project::print_diags(&bag) {
        eprintln!(
            "error: could not compile `{}` due to {} error(s)",
            target.display_name(),
            bag.error_count()
        );
        std::process::exit(1);
    }
    let out = out.expect("no errors implies compiled output");

    // mc_lower。
    let (spec, bag) = cql_compiler::mc_lower::lower_to_mc(&out, &sources, &config);
    if project::print_diags(&bag) {
        eprintln!(
            "error: could not lower `{}` to the model-checking fragment due to {} error(s)",
            target.display_name(),
            bag.error_count()
        );
        std::process::exit(2);
    }
    let mut spec = spec.expect("no errors implies a spec");

    // --bounded / --temporal property filter.
    let total_props = spec.properties.len();
    if bounded != temporal {
        spec.properties.retain(|p| match p.kind {
            cql_mc::ir::PropertyKind::Always(_) => bounded,
            cql_mc::ir::PropertyKind::Eventually(_) => temporal,
        });
    }
    println!(
        "verifying `{}` (stateright): {} table(s), {} transition(s), {} of {} propert(ies), k={}",
        target.display_name(),
        spec.tables.len(),
        spec.transitions.len(),
        spec.properties.len(),
        total_props,
        config.trace_length
    );

    let verdicts = cql_mc::stateright_be::check(&spec);
    let mut violated = 0usize;
    for v in &verdicts {
        println!("  {v}");
        if let cql_mc::counterexample::Verdict::Counterexample { cex, .. } = v {
            violated += 1;
            print!("{}", cex.render(&spec));
        }
    }
    let ok = verdicts.len() - violated;
    if violated > 0 {
        println!("result: {violated} propert(ies) violated, {ok} hold within the bounds");
        std::process::exit(1);
    }
    println!("result: all {} propert(ies) hold within the bounds", verdicts.len());
    Ok(())
}

fn cmd_clean(cwd: &Path, path: Option<&Path>) -> miette::Result<()> {
    let target = project::resolve_target(cwd, path).map_err(miette::Report::msg)?;
    let out_dir = target.out_dir(cwd);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir).into_diagnostic()?;
        println!("removed {}", out_dir.display());
    } else {
        println!("nothing to clean ({})", out_dir.display());
    }
    Ok(())
}
