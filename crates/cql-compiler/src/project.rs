//! Multi-module project compilation (doc/todo.md Phase 4).
//!
//! A project is a set of CQL source files whose `module` declarations name
//! them and whose `use` declarations form a dependency graph.
//! [`compile_project`] topologically sorts the graph (a cycle is a
//! diagnostic) and compiles each module with the real public interfaces of
//! its already-compiled dependencies — so cross-module calls are fully
//! type-checked. [`lower_project`] then lowers each module to CIR in the
//! same order, giving the lowerer the dependency interfaces so cross-module
//! references become qualified Rust paths (`crate::<module>::<item>`).
//!
//! [`ModuleInterface`] is the small typed summary of a compiled module that
//! flows between the two stages: names/kinds/params for name resolution
//! ([`ImportedItem`]) plus elaborated type signatures for the type checker
//! and CIR lowering ([`ImportedTypes`]).

use std::collections::HashMap;

use miette::NamedSource;

use crate::ast::{Item, Module, VariantPayload, Visibility};
use crate::cir::{lower_to_cir_with_imports, sanitize, CirImportModule, CirModule};
use crate::desugar::desugar_module;
use crate::diag::{CqlError, DiagBag};
use crate::lower::parse_module;
use crate::optimize::{optimize_module, OptimizedModule};
use crate::resolve::{ImportedItem, ImportedKind, ImportedModule};
use crate::types::{ImportSig, ImportedTypes};

/// The public interface of a compiled module, consumed by the compilation
/// of modules that `use` it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleInterface {
    /// Module path as matched against `use` declarations (single-segment:
    /// the module name).
    pub path: Vec<String>,
    /// Names/kinds/parameter names, consumed by name resolution.
    pub items: Vec<ImportedItem>,
    /// Elaborated type signatures, consumed by the type checker and CIR
    /// lowering.
    pub types: ImportedTypes,
}

impl ModuleInterface {
    /// The name-resolution view of this interface.
    pub fn as_imported_module(&self) -> ImportedModule {
        ImportedModule {
            path: self.path.clone(),
            public_items: self.items.clone(),
        }
    }
}

/// A successfully compiled project module.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledModule {
    /// Declared module name (`module <name>;`).
    pub name: String,
    /// Caller-supplied label (usually the source file path), for messages.
    pub label: String,
    pub module: OptimizedModule,
    pub interface: ModuleInterface,
}

/// The result of [`compile_project`]: all modules in dependency
/// (topological) order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOutput {
    pub modules: Vec<CompiledModule>,
}

/// Compile all modules of a project. `sources` is `(label, source text)`
/// pairs; the label identifies the file in diagnostics (the module name
/// comes from the source's `module` declaration).
///
/// Returns `Some(ProjectOutput)` only when every module compiled without
/// errors; warnings are returned alongside.
pub fn compile_project(sources: &[(String, String)]) -> (Option<ProjectOutput>, DiagBag) {
    let mut bag = DiagBag::new();

    // 1. Parse every source; collect declared names and `use` edges.
    struct Parsed {
        label: String,
        src: String,
        module: Module,
    }
    let mut parsed: Vec<Parsed> = Vec::new();
    for (label, src) in sources {
        match parse_module(src) {
            Ok(m) => parsed.push(Parsed {
                label: label.clone(),
                src: src.clone(),
                module: m,
            }),
            Err(b) => bag.merge(b),
        }
    }
    if bag.has_errors() {
        return (None, bag);
    }

    // 2. Duplicate module names are an error.
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (i, p) in parsed.iter().enumerate() {
        let name = &p.module.name.node;
        if let Some(&j) = by_name.get(name) {
            let src = NamedSource::new(format!("{name}.cql"), p.src.clone());
            bag.push_error(CqlError::new(
                src,
                p.module.name.span,
                format!("duplicate module name `{name}`"),
                Some(format!("also declared in `{}`", parsed[j].label)),
            ));
        } else {
            by_name.insert(name.clone(), i);
        }
    }
    if bag.has_errors() {
        return (None, bag);
    }

    // 3. Topological order (Kahn). Only edges to modules present in the
    //    project constrain the order; unresolved imports are reported by
    //    name resolution later.
    let uses: Vec<Vec<String>> = parsed
        .iter()
        .map(|p| {
            p.module
                .items
                .iter()
                .filter_map(|it| match it {
                    Item::Use(u) => Some(u.path.iter().map(|i| i.node.clone()).collect::<Vec<_>>().join("::")),
                    _ => None,
                })
                .filter(|path| by_name.contains_key(path))
                .collect()
        })
        .collect();
    let order = match topo_order(parsed.len(), &uses, &by_name) {
        Ok(o) => o,
        Err(cycle) => {
            for i in cycle {
                let p = &parsed[i];
                let name = &p.module.name.node;
                let src = NamedSource::new(format!("{name}.cql"), p.src.clone());
                bag.push_error(CqlError::new(
                    src,
                    p.module.name.span,
                    format!("module `{name}` is part of a circular `use` dependency"),
                    Some("break the cycle, e.g. by moving shared declarations into a third module".to_string()),
                ));
            }
            return (None, bag);
        }
    };

    // 4. Compile each module with the interfaces of its dependencies.
    let mut compiled: Vec<CompiledModule> = Vec::new();
    let mut interfaces: HashMap<String, ModuleInterface> = HashMap::new();
    for i in order {
        let p = &parsed[i];
        let deps: Vec<ModuleInterface> = uses[i]
            .iter()
            .filter_map(|path| interfaces.get(path).cloned())
            .collect();
        let (typed, b) = crate::lower::frontend_with_interfaces(&p.src, &deps);
        let module_ok = !b.has_errors();
        bag.merge(b);
        let name = p.module.name.node.clone();
        if let (Some(t), true) = (typed, module_ok) {
            let module = optimize_module(desugar_module(t));
            let interface = interface_of(&name, &module);
            interfaces.insert(name.clone(), interface.clone());
            compiled.push(CompiledModule {
                name,
                label: p.label.clone(),
                module,
                interface,
            });
        }
    }
    if bag.has_errors() {
        (None, bag)
    } else {
        (Some(ProjectOutput { modules: compiled }), bag)
    }
}

/// Kahn's algorithm over the `use` graph. Returns module indices in
/// dependency-first order, or `Err(cycle)` with the modules that could not
/// be ordered (the cycle and everything depending on it).
fn topo_order(
    n: usize,
    uses: &[Vec<String>],
    by_name: &HashMap<String, usize>,
) -> Result<Vec<usize>, Vec<usize>> {
    let mut indeg = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, deps) in uses.iter().enumerate() {
        for d in deps {
            if let Some(&j) = by_name.get(d) {
                indeg[i] += 1;
                dependents[j].push(i);
            }
        }
    }
    let mut queue: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = queue.pop() {
        order.push(i);
        for &j in &dependents[i] {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                queue.push(j);
            }
        }
    }
    if order.len() == n {
        Ok(order)
    } else {
        Err((0..n).filter(|&i| indeg[i] > 0).collect())
    }
}

/// Extract the public interface of a compiled module.
fn interface_of(name: &str, m: &OptimizedModule) -> ModuleInterface {
    let typed = &m.desugared.typed;
    let ast = &typed.resolved.module;
    let mut items = Vec::new();
    let mut types = ImportedTypes::default();
    for item in &ast.items {
        match item {
            Item::Operator(o) if o.vis == Visibility::Public => {
                let kind = match o.level {
                    crate::ast::EffectLevel::Function => ImportedKind::Function,
                    crate::ast::EffectLevel::Query => ImportedKind::Query,
                    crate::ast::EffectLevel::Action => ImportedKind::Action,
                };
                items.push(ImportedItem {
                    name: o.name.node.clone(),
                    kind,
                    params: Some(o.params.iter().map(|p| p.name.node.clone()).collect()),
                });
                if let Some(sig) = typed.operator_sigs.get(&o.name.node) {
                    types.ops.insert(
                        o.name.node.clone(),
                        ImportSig {
                            level: o.level,
                            type_params: o.type_params.iter().map(|p| p.node.clone()).collect(),
                            sig: sig.clone(),
                        },
                    );
                }
            }
            Item::Const(c) if c.vis == Visibility::Public => {
                items.push(ImportedItem {
                    name: c.name.node.clone(),
                    kind: ImportedKind::Const,
                    params: None,
                });
                if let Some(ty) = typed.expr_tys.get(&c.value.span) {
                    types.consts.insert(c.name.node.clone(), ty.clone());
                }
            }
            Item::Enum(e) if e.vis == Visibility::Public => {
                items.push(ImportedItem {
                    name: e.name.node.clone(),
                    kind: ImportedKind::Enum,
                    params: None,
                });
                for v in &e.variants {
                    let arity = match &v.payload {
                        VariantPayload::None => 0,
                        VariantPayload::Tuple(ts) => ts.len(),
                        VariantPayload::Record(_) => 1,
                    };
                    items.push(ImportedItem {
                        name: v.name.node.clone(),
                        kind: ImportedKind::EnumVariant { arity },
                        params: None,
                    });
                }
            }
            Item::TypeAlias(t) if t.vis == Visibility::Public => {
                items.push(ImportedItem {
                    name: t.name.node.clone(),
                    kind: ImportedKind::TypeAlias,
                    params: None,
                });
            }
            _ => {}
        }
    }
    ModuleInterface {
        path: vec![name.to_string()],
        items,
        types,
    }
}

/// Lower every compiled module to CIR in dependency order, supplying each
/// lowerer with the public interfaces of its dependencies so cross-module
/// references become `crate::<module>::<item>` paths.
pub fn lower_project(out: &ProjectOutput) -> Result<Vec<(String, CirModule)>, DiagBag> {
    let mut bag = DiagBag::new();
    let mut cir_imports: Vec<CirImportModule> = Vec::new();
    let mut lowered: Vec<(String, CirModule)> = Vec::new();
    for m in &out.modules {
        match lower_to_cir_with_imports(&m.module, &cir_imports) {
            Ok(cir) => {
                cir_imports.push(CirImportModule {
                    module: sanitize(&m.name),
                    ops: m.interface.types.ops.clone(),
                    consts: m.interface.types.consts.clone(),
                });
                lowered.push((m.name.clone(), cir));
            }
            Err(b) => bag.merge(b),
        }
    }
    bag.into_result(lowered)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const UTIL: &str = r#"
module util;

public function is_large_amount(x: float) -> bool == { x >= 100.0 }

public function clamp_nonnegative(x: float) -> float == {
    if x < 0.0 then 0.0 else x
}
"#;

    const SHOP: &str = r#"
module shop;

use util;

table orders { order_id: int, amount: float } primary key {order_id}

query large_orders() -> set<orders> == {
    read(orders, lambda(o) { is_large_amount(o.amount) })
}

query clamped() -> vector<float> == {
    [1.0, 2.0].map(clamp_nonnegative)
}
"#;

    fn sources() -> Vec<(String, String)> {
        vec![
            ("util.cql".to_string(), UTIL.to_string()),
            ("shop.cql".to_string(), SHOP.to_string()),
        ]
    }

    #[test]
    fn cross_module_typecheck() {
        let (out, bag) = compile_project(&sources());
        assert!(!bag.has_errors(), "{}", bag.render());
        let out = out.expect("project compiles");
        // Topological order: util before shop.
        let names: Vec<&str> = out.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["util", "shop"]);
        // The interface carries typed signatures.
        let util = &out.modules[0].interface;
        assert!(util.types.ops.contains_key("is_large_amount"));
    }

    #[test]
    fn cross_module_lower_qualifies_calls() {
        let (out, bag) = compile_project(&sources());
        assert!(!bag.has_errors(), "{}", bag.render());
        let lowered = lower_project(&out.expect("project compiles")).expect("lowers");
        let shop = &lowered.iter().find(|(n, _)| n == "shop").unwrap().1;
        let dbg = format!("{:?}", shop);
        assert!(
            dbg.contains("crate::util::is_large_amount"),
            "cross-module call must be qualified: {dbg}"
        );
        assert!(
            dbg.contains("crate::util::clamp_nonnegative"),
            "first-class imported fn ref must be qualified: {dbg}"
        );
    }

    #[test]
    fn cross_module_type_error_is_reported() {
        let bad = r#"
module shop;

use util;

query bad() -> bool == {
    is_large_amount(1)
}
"#;
        let sources = vec![
            ("util.cql".to_string(), UTIL.to_string()),
            ("shop.cql".to_string(), bad.to_string()),
        ];
        let (out, bag) = compile_project(&sources);
        assert!(out.is_none());
        assert!(bag.has_errors());
        let r = bag.render();
        assert!(r.contains("float") || r.contains("type"), "{r}");
    }

    #[test]
    fn cycle_is_a_diagnostic() {
        let a = "module a;\n\nuse b;\n\nquery qa() -> int == { 1 }\n";
        let b = "module b;\n\nuse a;\n\nquery qb() -> int == { 2 }\n";
        let sources = vec![
            ("a.cql".to_string(), a.to_string()),
            ("b.cql".to_string(), b.to_string()),
        ];
        let (out, bag) = compile_project(&sources);
        assert!(out.is_none());
        assert!(bag.has_errors());
        assert!(bag.render().contains("circular"), "{}", bag.render());
    }

    #[test]
    fn duplicate_module_names_are_a_diagnostic() {
        let sources = vec![
            ("a.cql".to_string(), "module dup;\n\nquery q() -> int == { 1 }\n".to_string()),
            ("b.cql".to_string(), "module dup;\n\nquery r() -> int == { 2 }\n".to_string()),
        ];
        let (out, bag) = compile_project(&sources);
        assert!(out.is_none());
        assert!(bag.render().contains("duplicate module name"), "{}", bag.render());
    }

    /// §2.2: `lookup` yields `option<value t>` (non-key fields); the runtime
    /// produces full rows. CIR lowering must bridge the views with a
    /// row→record coercion so the generated code type-checks.
    #[test]
    fn lookup_value_record_coercion_lowers() {
        let src = r#"
module shop;

table users { id: int, name: string, city: string } primary key {id}
table orders { order_id: int, user_id: int, amount: float }
    primary key {order_id}
    foreign key {user_id} references users

query totals() -> vector<{ key: string, agg: float }> == {
    aggregate(bag { (o, u) : o \in orders, u \in lookup(users, o.user_id) },
              group_key: lambda(p) { p.1.name },
              value: lambda(p) { p.0.amount },
              reducer: lambda(a, b) { a + b },
              init: 0.0,
              finalize: lambda(s) { s })
}
"#;
        let (out, bag) = compile_project(&[("shop.cql".to_string(), src.to_string())]);
        assert!(!bag.has_errors(), "{}", bag.render());
        let lowered = lower_project(&out.expect("project compiles")).expect("lowers");
        let dbg = format!("{:?}", lowered[0].1);
        assert!(
            dbg.contains("RecordLit"),
            "row→record coercion expected at the lookup boundary"
        );
    }
}
