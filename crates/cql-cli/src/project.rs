//! Project / single-file compilation driver: the shared logic behind
//! check / build / test / clean (doc/todo.md Phase 4).
//!
//! Target resolution rules:
//! - walk up from `path` (default: the current directory) looking for a
//!   cql.toml → project mode: compile every `**/*.cql` under `source_root`;
//! - no cql.toml found and `path` is a .cql file → zero-config single-file
//!   mode;
//! - otherwise, error out.
//!
//! Code generation (rust backend): each CQL module produces one Rust module
//! file (`src/<module>.rs`), and `src/lib.rs` collects the `pub mod` lines.
//! Cross-module references are already qualified as `crate::<module>::<item>`
//! at the CIR stage, so cross-module name collisions are naturally isolated
//! by Rust module namespaces. The generated Cargo.toml depends on
//! cql-runtime by **absolute path** and carries an empty `[workspace]`
//! section so it is not absorbed by a parent workspace.

use std::path::{Path, PathBuf};
use std::process::Command;

use cql_compiler::codegen::{Backend, EmitCtx, RustBackend};
use cql_compiler::diag::DiagBag;
use cql_compiler::project::{compile_project, lower_project, ProjectOutput};

use crate::manifest::{find_project_root, load_manifest, Manifest};

/// Compilation target: project mode or zero-config single-file mode.
pub enum Target {
    Project { root: PathBuf, manifest: Manifest },
    SingleFile { path: PathBuf },
}

impl Target {
    /// Name for display purposes (project name or file name).
    pub fn display_name(&self) -> String {
        match self {
            Target::Project { manifest, .. } => manifest.package.name.clone(),
            Target::SingleFile { path } => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module".to_string()),
        }
    }

    /// Output directory for generated code.
    pub fn out_dir(&self, cwd: &Path) -> PathBuf {
        match self {
            Target::Project { root, manifest } => root.join(&manifest.build.out_dir),
            Target::SingleFile { .. } => cwd.join("target").join("cql"),
        }
    }

    /// Backend name (command-line override takes precedence).
    pub fn backend(&self, override_backend: Option<&str>) -> String {
        match override_backend {
            Some(b) => b.to_string(),
            None => match self {
                Target::Project { manifest, .. } => manifest.build.backend.clone(),
                Target::SingleFile { .. } => "rust".to_string(),
            },
        }
    }
}

/// Resolve the compilation target (see module docs).
pub fn resolve_target(cwd: &Path, path: Option<&Path>) -> Result<Target, String> {
    let start = match path {
        Some(p) => {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
        None => cwd.to_path_buf(),
    };
    if let Some(root) = find_project_root(&start) {
        let manifest = load_manifest(&root)?;
        return Ok(Target::Project { root, manifest });
    }
    if start.is_file() && start.extension().map(|e| e == "cql").unwrap_or(false) {
        return Ok(Target::SingleFile { path: start });
    }
    Err(format!(
        "no cql.toml found from `{}` and it is not a .cql file; run `cqlc new <name>` to scaffold a project",
        start.display()
    ))
}

/// Collect compilation units: a `(label, source text)` list (sorted by path
/// for determinism).
pub fn collect_sources(target: &Target) -> Result<Vec<(String, String)>, String> {
    match target {
        Target::SingleFile { path } => {
            let src = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok(vec![(path.display().to_string(), src)])
        }
        Target::Project { root, manifest } => {
            let src_root = root.join(&manifest.build.source_root);
            if !src_root.is_dir() {
                return Err(format!("source_root not found: {}", src_root.display()));
            }
            let mut files = Vec::new();
            collect_cql_files(&src_root, &mut files)
                .map_err(|e| format!("cannot scan {}: {e}", src_root.display()))?;
            files.sort();
            if files.is_empty() {
                return Err(format!("no .cql files under {}", src_root.display()));
            }
            let mut out = Vec::new();
            for f in files {
                let src = std::fs::read_to_string(&f)
                    .map_err(|e| format!("cannot read {}: {e}", f.display()))?;
                let label = f
                    .strip_prefix(root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| f.display().to_string());
                out.push((label, src));
            }
            Ok(out)
        }
    }
}

fn collect_cql_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_cql_files(&path, out)?;
        } else if path.extension().map(|e| e == "cql").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// Run the full frontend + middle end (everything `check` does). The caller
/// is responsible for printing diagnostics.
pub fn check(target: &Target) -> Result<(Option<ProjectOutput>, DiagBag), String> {
    let sources = collect_sources(target)?;
    Ok(compile_project(&sources))
}

/// Print the diagnostic bag; returns whether it contains any errors.
pub fn print_diags(bag: &DiagBag) -> bool {
    if !bag.is_empty() {
        eprint!("{}", bag.render());
    }
    bag.has_errors()
}

/// Absolute path of the cql-runtime crate (cql-cli lives at
/// `<ws>/crates/cql-cli`).
fn runtime_crate_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cql-cli lives under <ws>/crates")
        .join("cql-runtime")
}

/// TOML path literal (converts Windows backslashes to forward slashes).
fn toml_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Derive a cargo package name: sanitize it, and steer clear of the Windows
/// UAC heuristic — a test binary whose name contains "update" is refused
/// with error 740.
fn cargo_pkg_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().next().unwrap().is_ascii_digit() {
        s = format!("cql_{s}");
    }
    s.replace("update", "upd")
}

/// Rust module file-name identifier (a CQL module name is already a valid
/// identifier; only Rust keywords need escaping).
fn rust_mod_name(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
        "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
        "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "union", "box", "try", "gen",
    ];
    if KEYWORDS.contains(&name) {
        format!("cql_{name}")
    } else {
        name.to_string()
    }
}

/// check + CIR lowering + Rust codegen: write a buildable cargo crate into
/// `out_dir`. On success, returns the list of module file names written.
pub fn emit_crate(target: &Target, out_dir: &Path) -> Result<Vec<String>, String> {
    let (out, bag) = check(target)?;
    let has_errors = print_diags(&bag);
    if has_errors {
        return Err(format!(
            "could not compile `{}` due to {} error(s)",
            target.display_name(),
            bag.error_count()
        ));
    }
    let out = out.expect("no errors implies compiled output");
    let lowered = match lower_project(&out) {
        Ok(l) => l,
        Err(bag) => {
            print_diags(&bag);
            return Err(format!(
                "could not lower `{}` due to {} error(s)",
                target.display_name(),
                bag.error_count()
            ));
        }
    };
    let backend = RustBackend;
    let mut modules: Vec<(String, String)> = Vec::new(); // (rust mod name, file text)
    for (name, cir) in &lowered {
        let ctx = EmitCtx::new(name.clone());
        let text = backend
            .emit(cir, &ctx)
            .map_err(|bag| {
                print_diags(&bag);
                format!("codegen failed for module `{name}`")
            })?;
        modules.push((rust_mod_name(name), text));
    }

    // Write the crate: Cargo.toml + src/lib.rs + src/<mod>.rs (keep target/
    // incremental build artifacts).
    let src_dir = out_dir.join("src");
    if src_dir.exists() {
        std::fs::remove_dir_all(&src_dir).map_err(|e| format!("cannot clean {}: {e}", src_dir.display()))?;
    }
    std::fs::create_dir_all(&src_dir).map_err(|e| format!("cannot create {}: {e}", src_dir.display()))?;
    let pkg = cargo_pkg_name(&target.display_name());
    let cargo_toml = format!(
        "[package]\nname = \"{pkg}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ncql-runtime = {{ path = \"{}\" }}\n\n\
         [workspace]\n",
        toml_path(&runtime_crate_path())
    );
    std::fs::write(out_dir.join("Cargo.toml"), cargo_toml)
        .map_err(|e| format!("cannot write Cargo.toml: {e}"))?;
    let lib_rs = format!(
        "// @generated by cqlc — do not edit.\n{}",
        modules
            .iter()
            .map(|(m, _)| format!("pub mod {m};\n"))
            .collect::<String>()
    );
    std::fs::write(src_dir.join("lib.rs"), lib_rs).map_err(|e| format!("cannot write lib.rs: {e}"))?;
    let mut names = Vec::new();
    for (mod_name, text) in modules {
        std::fs::write(src_dir.join(format!("{mod_name}.rs")), text)
            .map_err(|e| format!("cannot write {mod_name}.rs: {e}"))?;
        names.push(mod_name);
    }
    Ok(names)
}

/// `--backend mududb`: placeholder deployment plan (cql-compiler `mududb_be`,
/// proposal stage, contains no syscall numbers; see doc/backend-mududb.md
/// §3/§9). Writes one `<module>.mududb-plan.txt` per module into `out_dir`.
pub fn emit_mududb_plan(target: &Target, out_dir: &Path) -> Result<Vec<String>, String> {
    use cql_compiler::mududb_be::MududbBackend;

    let (out, bag) = check(target)?;
    let has_errors = print_diags(&bag);
    if has_errors {
        return Err(format!(
            "could not compile `{}` due to {} error(s)",
            target.display_name(),
            bag.error_count()
        ));
    }
    let out = out.expect("no errors implies compiled output");
    let lowered = match lower_project(&out) {
        Ok(l) => l,
        Err(bag) => {
            print_diags(&bag);
            return Err(format!(
                "could not lower `{}` due to {} error(s)",
                target.display_name(),
                bag.error_count()
            ));
        }
    };
    std::fs::create_dir_all(out_dir).map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
    let backend = MududbBackend;
    let mut names = Vec::new();
    for (name, cir) in &lowered {
        let ctx = EmitCtx::new(name.clone());
        let plan = backend
            .emit(cir, &ctx)
            .map_err(|bag| {
                print_diags(&bag);
                format!("mududb plan emission failed for module `{name}`")
            })?;
        let file = format!("{name}.mududb-plan.txt");
        std::fs::write(out_dir.join(&file), plan)
            .map_err(|e| format!("cannot write {file}: {e}"))?;
        names.push(name.clone());
    }
    Ok(names)
}

/// Run `cargo <sub> --offline` inside `out_dir`, forwarding output; returns
/// whether cargo succeeded.
pub fn run_cargo(out_dir: &Path, sub: &str) -> Result<bool, String> {
    let status = Command::new("cargo")
        .arg(sub)
        .arg("--offline")
        .current_dir(out_dir)
        .status()
        .map_err(|e| format!("failed to spawn `cargo {sub}` in {}: {e}", out_dir.display()))?;
    Ok(status.success())
}
