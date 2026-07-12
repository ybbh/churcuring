//! E2E tests for the cqlc CLI (Phase 4).
//!
//! Example projects are copied into `target/tmp/cql_cli_tests/<case>`
//! (recreated fresh each run) so `out_dir` writes never dirty `examples/`.
//! NOTE: test names must not contain "update" — on Windows, test binaries
//! whose names contain that substring fail to spawn (UAC heuristic, error
//! 740).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cqlc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cqlc"))
}

/// Workspace root: cql-cli lives at `<ws>/crates/cql-cli`.
fn ws_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("cql-cli lives under <ws>/crates")
        .to_path_buf()
}

/// A fresh per-case scratch directory under target/tmp/cql_cli_tests.
fn fresh_case(name: &str) -> PathBuf {
    let dir = ws_root().join("target/tmp/cql_cli_tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create case dir");
    dir
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Shared cargo target dir so the generated crates reuse dependency
/// artifacts across cases (cargo serializes on the lock).
fn shared_cargo_target() -> PathBuf {
    ws_root().join("target/tmp/cql_cli_tests/cargo_target")
}

fn run(args: &[&str], cwd: &Path) -> Output {
    cqlc()
        .args(args)
        .current_dir(cwd)
        .env("CARGO_TARGET_DIR", shared_cargo_target())
        .output()
        .expect("spawn cqlc")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn copy_example(case: &Path, name: &str) -> PathBuf {
    let dst = case.join(name);
    copy_dir(&ws_root().join("examples").join(name), &dst);
    dst
}

#[test]
fn check_shop_project_multi_module() {
    let case = fresh_case("check_shop");
    let shop = copy_example(&case, "shop_project");
    let out = run(&["check", shop.to_str().unwrap()], &case);
    assert!(
        out.status.success(),
        "check shop_project failed:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    // Two modules (shop + util) compiled; the cross-module call to
    // `is_large_amount` type-checked against util's public interface.
    assert!(
        stdout(&out).contains("2 module(s)"),
        "unexpected output: {}",
        stdout(&out)
    );
}

#[test]
fn check_bank_project() {
    let case = fresh_case("check_bank");
    let bank = copy_example(&case, "bank_project");
    let out = run(&["check", bank.to_str().unwrap()], &case);
    assert!(
        out.status.success(),
        "check bank_project failed:\n{}",
        stderr(&out)
    );
}

#[test]
fn check_analytics_single_file() {
    let case = fresh_case("check_analytics");
    let analytics = case.join("analytics.cql");
    std::fs::copy(ws_root().join("examples/analytics.cql"), &analytics).unwrap();
    let out = run(&["check", analytics.to_str().unwrap()], &case);
    assert!(
        out.status.success(),
        "check analytics.cql failed:\n{}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("analytics"), "{}", stdout(&out));
}

#[test]
fn build_shop_project_crate_builds_offline() {
    let case = fresh_case("build_shop");
    let shop = copy_example(&case, "shop_project");
    let out = run(&["build", shop.to_str().unwrap()], &case);
    assert!(
        out.status.success(),
        "build shop_project failed:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("cargo build succeeded"),
        "{}",
        stdout(&out)
    );
    // Generated crate layout: Cargo.toml + lib.rs + one file per module.
    let gen = shop.join("target/cql");
    assert!(gen.join("Cargo.toml").is_file());
    assert!(gen.join("src/lib.rs").is_file());
    assert!(gen.join("src/shop.rs").is_file());
    assert!(gen.join("src/util.rs").is_file());
    let shop_rs = std::fs::read_to_string(gen.join("src/shop.rs")).unwrap();
    assert!(
        shop_rs.contains("crate::util::is_large_amount"),
        "cross-module call must be qualified"
    );
    // clean removes out_dir.
    let out = run(&["clean", shop.to_str().unwrap()], &case);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!gen.exists(), "clean must remove out_dir");
}

#[test]
fn test_bank_project_cql_test_block_passes() {
    let case = fresh_case("test_bank");
    let bank = copy_example(&case, "bank_project");
    let out = run(&["test", bank.to_str().unwrap()], &case);
    assert!(
        out.status.success(),
        "test bank_project failed:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("test_transfer_basic") && combined.contains("ok"),
        "expected test_transfer_basic to pass:\n{combined}"
    );
}

#[test]
fn check_type_error_exits_1_with_diagnostic() {
    let case = fresh_case("type_error");
    let bad = case.join("bad.cql");
    std::fs::write(
        &bad,
        "module bad;\n\nquery wrong() -> int == { \"not an int\" }\n",
    )
    .unwrap();
    let out = run(&["check", bad.to_str().unwrap()], &case);
    assert_eq!(out.status.code(), Some(1), "expected exit code 1");
    let err = stderr(&out);
    assert!(err.contains("type mismatch"), "stderr: {err}");
    assert!(err.contains("bad.cql"), "stderr: {err}");
}

#[test]
fn new_scaffold_then_check_passes() {
    let case = fresh_case("new_scaffold");
    let out = run(&["new", "foo"], &case);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(case.join("foo/cql.toml").is_file());
    assert!(case.join("foo/src/main.cql").is_file());
    let out = run(&["check", "foo"], &case);
    assert!(
        out.status.success(),
        "check of scaffold failed:\n{}",
        stderr(&out)
    );
}

#[test]
fn build_mududb_backend_emits_placeholder_plan() {
    let case = fresh_case("mududb");
    let analytics = case.join("analytics.cql");
    std::fs::copy(ws_root().join("examples/analytics.cql"), &analytics).unwrap();
    let out = run(
        &["build", "--backend", "mududb", analytics.to_str().unwrap()],
        &case,
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("deployment plan (PROPOSAL)"),
        "stdout: {}",
        stdout(&out)
    );
    // Single-file mode: out_dir = <cwd>/target/cql; one plan file per module.
    let plan = case.join("target/cql/analytics.mududb-plan.txt");
    assert!(plan.is_file(), "plan file missing at {}", plan.display());
    let text = std::fs::read_to_string(&plan).unwrap();
    assert!(text.contains("mududb_syscall_v1"), "plan: {text}");
    assert!(text.contains("component analytics {"), "plan: {text}");
}

// ---- Phase 5: cqlc verify ----

#[test]
fn verify_bank_project_proves_properties() {
    let case = fresh_case("verify_bank");
    let bank = copy_example(&case, "bank_project");
    let out = run(&["verify", bank.to_str().unwrap()], &case);
    assert_eq!(
        out.status.code(),
        Some(0),
        "verify bank_project failed:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let out_text = stdout(&out);
    assert!(
        out_text.contains("PROVED") && out_text.contains("balance_conserved"),
        "stdout: {out_text}"
    );
    assert!(out_text.contains("no_negative"), "stdout: {out_text}");
    // The prime property is skipped with a warning, not an error.
    assert!(
        stderr(&out).contains("transfer_preserves"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn verify_violated_invariant_exits_1_with_counterexample() {
    let case = fresh_case("verify_buggy");
    let proj = case.join("buggy_bank");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(
        proj.join("cql.toml"),
        "[package]\nname = \"buggy_bank\"\nversion = \"0.1.0\"\n\n[build]\nsource_root = \"src\"\n",
    )
    .unwrap();
    std::fs::write(
        proj.join("verify.toml"),
        "[domain]\naccounts.rows = 2\naccounts.id = \"1..2\"\naccounts.balance = [0, 6000, 4000]\n",
    )
    .unwrap();
    // Bug: the credit leg debits too — total balance is not conserved.
    let bank_src = std::fs::read_to_string(
        ws_root().join("examples/bank_project/src/bank.cql"),
    )
    .unwrap()
    .replace("v.balance + amt } }) }", "v.balance - amt } }) }");
    std::fs::write(proj.join("src/bank.cql"), bank_src).unwrap();

    let out = run(&["verify", proj.to_str().unwrap()], &case);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on violation:\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let out_text = stdout(&out);
    assert!(
        out_text.contains("COUNTEREXAMPLE balance_conserved"),
        "stdout: {out_text}"
    );
    // Counterexample trace: states rendered as table snapshots.
    assert!(
        out_text.contains("counterexample for `balance_conserved`")
            && out_text.contains("accounts {")
            && out_text.contains("transfer("),
        "stdout: {out_text}"
    );
}

#[test]
fn verify_z3_engine_is_friendly_error() {
    let case = fresh_case("verify_z3");
    let bank = copy_example(&case, "bank_project");
    let out = run(
        &["verify", "--engine", "z3", bank.to_str().unwrap()],
        &case,
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not available"),
        "stderr: {}",
        stderr(&out)
    );
}
