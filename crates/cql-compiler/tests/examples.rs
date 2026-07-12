//! End-to-end tests over the bundled example projects (doc/cql.md §8):
//! parse → resolve → effect → types → terminate → desugar → optimize must
//! succeed with zero errors and zero warnings.

use cql_compiler::optimize::ReadPlan;
use cql_compiler::pipeline;
use cql_compiler::resolve::{ImportedItem, ImportedKind, ImportedModule};

fn example(path: &str) -> String {
    let full = format!("{}/../../examples/{}", env!("CARGO_MANIFEST_DIR"), path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {}", full, e))
}

fn compile_clean(src: &str, imports: &[ImportedModule]) -> cql_compiler::optimize::OptimizedModule {
    let (opt, bag) = pipeline::compile_module_with_imports(src, imports);
    assert!(
        !bag.has_errors() && bag.warning_count() == 0,
        "diagnostics:\n{}",
        bag.render()
    );
    opt.expect("optimized module")
}

#[test]
fn analytics_example_compiles_clean() {
    let src = example("analytics.cql");
    let m = compile_clean(&src, &[]);

    // §8.2: the sessions read constrained on `user_id` uses the declared
    // secondary index; the pk-constrained reads are point lookups.
    let index_scans: Vec<_> = m
        .plans
        .values()
        .filter(|p| {
            matches!(p, ReadPlan::IndexScan { index } if index.node == "sessions_by_user")
        })
        .collect();
    assert_eq!(index_scans.len(), 1, "plans: {:?}", m.plans);
    assert!(
        m.plans.values().any(|p| *p == ReadPlan::PointLookup),
        "expected a point lookup in plans: {:?}",
        m.plans
    );
}

#[test]
fn bank_example_compiles_clean() {
    let src = example("bank_project/src/bank.cql");
    compile_clean(&src, &[]);
}

#[test]
fn shop_example_compiles_clean_with_util_import() {
    let util_src = example("shop_project/src/util.cql");
    let (util_typed, bag) = cql_compiler::lower::frontend(&util_src);
    assert!(!bag.has_errors(), "util: {}", bag.render());
    let util_typed = util_typed.expect("util typed");

    // The driver-derived import descriptor for `use util;`: public functions
    // with their parameter names (for named-argument checking).
    let mut public_items = Vec::new();
    for item in &util_typed.resolved.module.items {
        if let cql_compiler::ast::Item::Operator(op) = item {
            if op.vis == cql_compiler::ast::Visibility::Public {
                public_items.push(ImportedItem {
                    name: op.name.node.clone(),
                    kind: match op.level {
                        cql_compiler::ast::EffectLevel::Function => ImportedKind::Function,
                        cql_compiler::ast::EffectLevel::Query => ImportedKind::Query,
                        cql_compiler::ast::EffectLevel::Action => ImportedKind::Action,
                    },
                    params: Some(op.params.iter().map(|p| p.name.node.clone()).collect()),
                });
            }
        }
    }
    assert_eq!(public_items.len(), 2, "util exports two public functions");
    let imports = [ImportedModule {
        path: vec!["util".to_string()],
        public_items,
    }];

    let shop_src = example("shop_project/src/shop.cql");
    let m = compile_clean(&shop_src, &imports);

    // shop.cql reads users by primary key (via `lookup`) at least once.
    assert!(
        m.plans.values().any(|p| *p == ReadPlan::PointLookup),
        "plans: {:?}",
        m.plans
    );
}

#[test]
fn missing_import_is_an_error() {
    let shop_src = example("shop_project/src/shop.cql");
    let (opt, bag) = pipeline::compile_module(&shop_src);
    assert!(opt.is_none());
    assert!(bag.has_errors());
}
