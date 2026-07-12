//! Codegen tests: CIR → Rust source via the askama-backed `RustBackend`.
//!
//! The end-to-end tests compile `examples/analytics.cql` and
//! `examples/bank_project/src/bank.cql` to Rust, assemble a scratch cargo
//! project under `target/tmp/` depending on `cql-runtime` by relative path,
//! and run the generated `#[test]`s with `cargo test`.

use std::path::PathBuf;
use std::process::Command;

use cql_compiler::cir::lower_to_cir;
use cql_compiler::codegen::{Backend, EmitCtx, RustBackend};
use cql_compiler::pipeline;

fn render(src: &str) -> String {
    let (opt, bag) = pipeline::compile_module(src);
    assert!(!bag.has_errors(), "pipeline diagnostics:\n{}", bag.render());
    let cir = lower_to_cir(&opt.expect("optimized module"))
        .unwrap_or_else(|b| panic!("lowering diagnostics:\n{}", b.render()));
    RustBackend
        .emit(&cir, &EmitCtx::new("test"))
        .unwrap_or_else(|b| panic!("codegen diagnostics:\n{}", b.render()))
}

fn example(path: &str) -> String {
    let full = format!("{}/../../examples/{}", env!("CARGO_MANIFEST_DIR"), path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {}", full, e))
}

// --- unit-level spot checks -------------------------------------------------

#[test]
fn emits_state_and_operators() {
    let src = "module t;
         table users { id: int, name: string } primary key {id}
         query names() -> set<string> == { set { u.name : u \\in users } }";
    let out = render(src);
    assert!(out.contains("pub struct UsersRow"), "row struct:\n{out}");
    assert!(out.contains("pub struct UsersKey"), "key struct:\n{out}");
    assert!(out.contains("pub struct State"), "state:\n{out}");
    assert!(out.contains("pub fn names(state: &State) -> CqlSet<String>"), "query:\n{out}");
    assert!(out.contains("Rc::new(move |__arg|"), "closure:\n{out}");
}

#[test]
fn emits_lambda_lifting_env() {
    let src = "module t;
         function add_n(n: int, xs: vector<int>) -> vector<int> == {
             xs.map(lambda [n](x) { x + n })
         }";
    let out = render(src);
    assert!(out.contains("__lift_0_Env"), "env struct:\n{out}");
    assert!(out.contains("n: i64"), "env field:\n{out}");
    assert!(out.contains("fn add_n(n: i64, xs: Vec<i64>) -> Vec<i64>"), "fn:\n{out}");
}

#[test]
fn emits_enum_with_boxing() {
    let src = "module t;
         enum tree { leaf(int), node(tree, int, tree) }
         function depth(t: tree) -> int == {
             match t {
                 leaf(_) => 0,
                 node(l, _, r) => 1 + max(depth(l), depth(r)),
             }
         }";
    let out = render(src);
    assert!(
        out.contains("Node(Box<Tree>, i64, Box<Tree>)"),
        "boxed recursive payloads:\n{out}"
    );
    assert!(out.contains("match"), "match:\n{out}");
}

#[test]
fn emits_write_ops() {
    let src = "module t;
         table accounts { id: int, bal: int } primary key {id}
         action deposit(id: int, amt: int) -> set<write_op> == {
             set { update(accounts, id, lambda [amt](a) { record { a with bal: a.bal + amt } }) }
         }";
    let out = render(src);
    assert!(out.contains("WriteOp::Update"), "update op:\n{out}");
    assert!(out.contains("ClosureFunVal::new"), "transform:\n{out}");
    assert!(out.contains("pub fn apply"), "state apply:\n{out}");
}

// --- end-to-end: generated Rust compiles and its tests pass ------------------

/// Assemble `<tmp>/cql_codegen_<name>` as a scratch binary crate with the
/// generated module as `src/lib.rs` and run `cargo test` in it.
fn cargo_test_generated(name: &str, rust_src: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tmp = root.join("target/tmp");
    std::fs::create_dir_all(&tmp).expect("create target/tmp");
    let proj = tmp.join(format!("cql_codegen_{name}"));
    if proj.exists() {
        std::fs::remove_dir_all(&proj).expect("clean scratch project");
    }
    std::fs::create_dir_all(proj.join("src")).expect("create scratch src");
    // The empty [workspace] detaches the scratch project from the outer one.
    // cql-runtime path from target/tmp/cql_codegen_<name>: ../../../crates/cql-runtime
    std::fs::write(
        proj.join("Cargo.toml"),
        "[package]\nname = \"cql_gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ncql-runtime = { path = \"../../../crates/cql-runtime\" }\n\n\
         [workspace]\n",
    )
    .expect("write scratch Cargo.toml");
    std::fs::write(proj.join("src/lib.rs"), rust_src).expect("write generated lib.rs");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(&cargo)
        .arg("test")
        .arg("--offline")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&proj)
        .output()
        .expect("spawn cargo test");
    assert!(
        out.status.success(),
        "cargo test failed in {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        proj.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn analytics_compiles_and_runs() {
    let src = example("analytics.cql");
    let rust = render(&src);
    cargo_test_generated("analytics", &rust);
}

#[test]
fn bank_compiles_and_runs() {
    let src = example("bank_project/src/bank.cql");
    let rust = render(&src);
    cargo_test_generated("bank", &rust);
}
