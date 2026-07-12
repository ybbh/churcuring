//! Lowering: tree-sitter CST → surface AST, plus the `frontend` entry point
//! (doc/cql.md appendix A; pipeline §D.3).
//!
//! [`parse_module`] parses source text with the tree-sitter grammar
//! (`crates/tree-sitter-cql`), reports every `ERROR`/`MISSING` node as a
//! diagnostic, and recursively lowers the concrete syntax tree into
//! [`ast::Module`]. All spans are byte ranges taken from the CST nodes.
//!
//! Literal handling split of responsibilities:
//!
//! - Lowering **parses and normalizes**: int literals (decimal/`0x` hex,
//!   `_` separators, `i64` overflow errors), float literals, string escapes
//!   (`\n \t \\ \" \u{...}`) and interpolation, decimal `repr`
//!   normalization, and the date literal's `YYYY-MM-DD` shape.
//! - **Semantic validity** of decimal precisions (significant/fractional
//!   digits vs `decimal(m, n)`) and calendar dates (leap years etc.) is
//!   checked by the type checker ([`crate::types`]), which already owns those
//!   diagnostics — lowering does not duplicate them.
//!
//! [`frontend`] chains the full semantic front-end: parse → resolve → effect
//! → types → terminate, accumulating all diagnostics. Passes that abort
//! (parse/resolve/effect) short-circuit the chain; the type checker and
//! termination pass always run to completion over whatever resolved.

use miette::NamedSource;
use tree_sitter::{Node, Parser, Tree};

use crate::ast::*;
use crate::diag::{CqlError, DiagBag};
use crate::resolve::{resolve_module_with_src, ImportedModule};
use crate::types::{check_module_with_src, TypedModule};
use crate::{effect, terminate};

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Parse and lower a CQL module. Any syntax error (`ERROR`/`MISSING` node)
/// or malformed literal yields `Err` with all collected diagnostics.
pub fn parse_module(src: &str) -> Result<Module, DiagBag> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cql::LANGUAGE.into())
        .expect("tree-sitter CQL language version mismatch");
    let tree = parser.parse(src, None).expect("tree-sitter never fails to parse");
    let named = NamedSource::new("input.cql", src.to_string());
    let mut l = Lower { src, named, diags: DiagBag::new() };
    l.check_syntax_errors(&tree);
    if l.diags.has_errors() {
        return Err(l.diags);
    }
    let module = l.lower_module(tree.root_node());
    l.diags.into_result(module)
}

/// The full front-end: parse → resolve → effect → types → terminate, with no
/// imported modules. Returns the typed module only when no errors were
/// reported by any pass (warnings do not block).
pub fn frontend(src: &str) -> (Option<TypedModule>, DiagBag) {
    frontend_with_imports(src, &[])
}

/// Like [`frontend`], but with already-resolved imported modules supplied by
/// the driver (multi-module projects).
pub fn frontend_with_imports(src: &str, imports: &[ImportedModule]) -> (Option<TypedModule>, DiagBag) {
    let module = match parse_module(src) {
        Ok(m) => m,
        Err(bag) => return (None, bag),
    };
    let named = NamedSource::new(format!("{}.cql", module.name.node), src.to_string());
    let resolved = match resolve_module_with_src(module, imports, named.clone()) {
        Ok(r) => r,
        Err(bag) => return (None, bag),
    };
    if let Err(bag) = effect::check_effects_with_src(&resolved, named.clone()) {
        return (None, bag);
    }
    let (typed, mut bag) = check_module_with_src(&resolved, named.clone());
    if let Err(tbag) = terminate::check_termination_with_src(&resolved, named) {
        bag.merge(tbag);
    }
    if bag.has_errors() {
        (None, bag)
    } else {
        (Some(typed), bag)
    }
}

/// Like [`frontend_with_imports`], but with full typed module interfaces:
/// cross-module calls are type-checked against the dependencies' public
/// operator signatures (multi-module projects, [`crate::project`]).
pub fn frontend_with_interfaces(
    src: &str,
    interfaces: &[crate::project::ModuleInterface],
) -> (Option<TypedModule>, DiagBag) {
    let imports: Vec<ImportedModule> = interfaces.iter().map(|i| i.as_imported_module()).collect();
    let mut imported_types = crate::types::ImportedTypes::default();
    for i in interfaces {
        imported_types.ops.extend(i.types.ops.clone());
        imported_types.consts.extend(i.types.consts.clone());
    }
    let module = match parse_module(src) {
        Ok(m) => m,
        Err(bag) => return (None, bag),
    };
    let named = NamedSource::new(format!("{}.cql", module.name.node), src.to_string());
    let resolved = match resolve_module_with_src(module, &imports, named.clone()) {
        Ok(r) => r,
        Err(bag) => return (None, bag),
    };
    if let Err(bag) = effect::check_effects_with_src(&resolved, named.clone()) {
        return (None, bag);
    }
    let (typed, mut bag) =
        crate::types::check_module_with_imports(&resolved, named.clone(), &imported_types);
    if let Err(tbag) = terminate::check_termination_with_src(&resolved, named) {
        bag.merge(tbag);
    }
    if bag.has_errors() {
        (None, bag)
    } else {
        (Some(typed), bag)
    }
}

// ---------------------------------------------------------------------------
// Lowerer
// ---------------------------------------------------------------------------

struct Lower<'a> {
    src: &'a str,
    named: NamedSource<String>,
    diags: DiagBag,
}

type N<'t> = Node<'t>;

impl<'a> Lower<'a> {
    // ---- small helpers ----------------------------------------------------

    fn span<'t>(&self, n: N<'t>) -> Span {
        Span { start: n.start_byte() as u32, end: n.end_byte() as u32 }
    }

    fn text<'t>(&self, n: N<'t>) -> &'a str {
        n.utf8_text(self.src.as_bytes()).expect("grammar guarantees valid UTF-8")
    }

    fn err(&mut self, span: Span, message: impl Into<String>, help: Option<String>) {
        self.diags.push_error(CqlError::new(self.named.clone(), span, message, help));
    }

    fn ident<'t>(&self, n: N<'t>) -> Ident {
        Spanned::new(self.text(n).to_string(), self.span(n))
    }

    /// The single named child of a wrapper node (`type`, `literal`, `pattern`,
    /// `temporal_expression`), or the node itself.
    fn unwrap<'b>(&self, n: N<'b>) -> N<'b> {
        match n.kind() {
            "type" | "literal" | "pattern" | "temporal_expression" => {
                n.named_child(0).unwrap_or(n)
            }
            _ => n,
        }
    }

    fn field<'t>(&self, n: N<'t>, name: &str) -> Option<N<'t>> {
        n.child_by_field_name(name)
    }

    fn children_of<'t>(&self, n: N<'t>, kind: &str) -> Vec<N<'t>> {
        (0..n.child_count() as u32)
            .filter_map(|i| n.child(i))
            .filter(|c| c.kind() == kind)
            .collect()
    }

    fn all_children<'t>(&self, n: N<'t>) -> Vec<N<'t>> {
        (0..n.child_count() as u32).filter_map(|i| n.child(i)).collect()
    }

    fn named_children<'t>(&self, n: N<'t>) -> Vec<N<'t>> {
        (0..n.named_child_count() as u32).filter_map(|i| n.named_child(i)).collect()
    }

    fn has_child<'t>(&self, n: N<'t>, kind: &str) -> bool {
        (0..n.child_count() as u32).filter_map(|i| n.child(i)).any(|c| c.kind() == kind)
    }

    // ---- syntax-error scan -------------------------------------------------

    /// Walk the whole tree collecting `ERROR` and `MISSING` nodes.
    fn check_syntax_errors(&mut self, tree: &Tree) {
        fn walk<'t>(l: &mut Lower, n: N<'t>) {
            if n.kind() == "ERROR" {
                l.err(l.span(n), "syntax error", None);
                return; // do not descend: the whole region is one error
            }
            if n.is_missing() {
                l.err(
                    l.span(n),
                    format!("syntax error: missing `{}`", n.kind()),
                    None,
                );
                return;
            }
            for c in (0..n.child_count() as u32).filter_map(|i| n.child(i)) {
                walk(l, c);
            }
        }
        walk(self, tree.root_node());
    }

    // ---- module & items ----------------------------------------------------

    fn lower_module<'t>(&mut self, root: N<'t>) -> Module {
        let name = self
            .field(root, "name")
            .map(|n| self.ident(n))
            .unwrap_or_else(|| Spanned::new("<anonymous>".to_string(), self.span(root)));
        let mut items = Vec::new();
        for child in (0..root.child_count() as u32).filter_map(|i| root.child(i)) {
            if let Some(item) = self.lower_item(child) {
                items.push(item);
            }
        }
        Module { name, items, span: self.span(root) }
    }

    fn lower_item<'t>(&mut self, n: N<'t>) -> Option<Item> {
        let item = match n.kind() {
            "use_declaration" => {
                // The optional alias is a field; exclude it from the path.
                let alias_node = self.field(n, "alias");
                let mut path = Vec::new();
                for c in self.all_children(n) {
                    if c.kind() == "ident" && alias_node.map(|a| a.id()) != Some(c.id()) {
                        path.push(self.ident(c));
                    }
                }
                let alias = alias_node.map(|a| self.ident(a));
                Item::Use(UseDecl { path, alias })
            }
            "const_declaration" => Item::Const(ConstDecl {
                vis: self.visibility(n),
                name: self.ident(self.field(n, "name")?),
                ty: self.lower_type(self.field(n, "type")?),
                value: self.lower_expr(self.field(n, "value")?),
            }),
            "type_declaration" => Item::TypeAlias(TypeAliasDecl {
                vis: self.visibility(n),
                name: self.ident(self.field(n, "name")?),
                params: self.type_params(n),
                ty: self.lower_type(self.field(n, "definition")?),
            }),
            "enum_declaration" => {
                let variants = self
                    .children_of(n, "variant")
                    .into_iter()
                    .map(|v| self.lower_variant(v))
                    .collect();
                Item::Enum(EnumDecl {
                    vis: self.visibility(n),
                    name: self.ident(self.field(n, "name")?),
                    params: self.type_params(n),
                    variants,
                })
            }
            "table_declaration" => {
                let schema = self.field(n, "schema")?;
                let fields = self
                    .children_of(schema, "field_declaration")
                    .into_iter()
                    .map(|fd| {
                        (
                            self.ident(self.field(fd, "name").expect("grammar guarantees field name")),
                            self.lower_type(self.field(fd, "type").expect("grammar guarantees field type")),
                        )
                    })
                    .collect();
                let pk = match self.children_of(n, "primary_key_clause").first() {
                    Some(pk) => self.children_of(*pk, "ident").iter().map(|i| self.ident(*i)).collect(),
                    None => vec![],
                };
                let fks = self
                    .children_of(n, "foreign_key_clause")
                    .iter()
                    .map(|fk| {
                        // The `target` ident is a field; the rest are the columns.
                        let target = self.field(*fk, "target").expect("grammar guarantees fk target");
                        let cols: Vec<Ident> = self
                            .children_of(*fk, "ident")
                            .iter()
                            .filter(|i| i.id() != target.id())
                            .map(|i| self.ident(*i))
                            .collect();
                        FkClause { cols, references: self.ident(target) }
                    })
                    .collect();
                Item::Table(TableDecl { vis: self.visibility(n), name: self.ident(self.field(n, "name")?), fields, pk, fks })
            }
            "index_declaration" => {
                // `name` and `table` are fields; the remaining idents are the columns.
                let name_node = self.field(n, "name")?;
                let table_node = self.field(n, "table")?;
                let mut cols = Vec::new();
                for c in self.all_children(n) {
                    if c.kind() == "ident" && c.id() != name_node.id() && c.id() != table_node.id() {
                        cols.push(self.ident(c));
                    }
                }
                Item::Index(IndexDecl {
                    vis: self.visibility(n),
                    name: self.ident(name_node),
                    table: self.ident(table_node),
                    cols,
                })
            }
            "function_declaration" | "query_declaration" | "action_declaration" => {
                self.lower_operator(n)
            }
            "invariant_declaration" => Item::Invariant(InvariantDecl {
                name: self.ident(self.field(n, "name")?),
                table: self.ident(self.field(n, "table")?),
                body: self.lower_expr(self.field(n, "condition")?),
            }),
            "test_declaration" => {
                let mut stmts = Vec::new();
                for c in self.all_children(n) {
                    match c.kind() {
                        "fixture_statement" => stmts.push(TestStmt::Fixture {
                            table: self.ident(self.field(c, "name").expect("grammar guarantees fixture name")),
                            rows: self.lower_expr(self.field(c, "value").expect("grammar guarantees fixture value")),
                        }),
                        "expect_statement" => stmts.push(TestStmt::Expect {
                            lhs: self.lower_expr(self.field(c, "actual").expect("grammar guarantees expect lhs")),
                            rhs: self.lower_expr(self.field(c, "expected").expect("grammar guarantees expect rhs")),
                        }),
                        _ => {}
                    }
                }
                Item::Test(TestDecl { name: self.ident(self.field(n, "name")?), stmts })
            }
            "property_declaration" => Item::Property(PropertyDecl {
                name: self.ident(self.field(n, "name")?),
                body: self.lower_temporal(self.field(n, "body")?),
            }),
            "fairness_declaration" => {
                let kind = match self.field(n, "kind").map(|k| self.text(k)) {
                    Some("strong") => FairnessKind::Strong,
                    _ => FairnessKind::Weak,
                };
                let mut actions = Vec::new();
                for c in self.all_children(n) {
                    if c.kind() == "ident" {
                        actions.push(self.ident(c));
                    }
                }
                Item::Fairness(FairnessDecl { kind, actions })
            }
            _ => return None,
        };
        Some(item)
    }

    fn visibility<'t>(&self, n: N<'t>) -> Visibility {
        if self.has_child(n, "visibility") {
            Visibility::Public
        } else {
            Visibility::Private
        }
    }

    fn type_params<'t>(&mut self, n: N<'t>) -> Vec<Ident> {
        match self.field(n, "type_parameters") {
            Some(tp) => self.children_of(tp, "ident").iter().map(|i| self.ident(*i)).collect(),
            None => vec![],
        }
    }

    fn lower_variant<'t>(&mut self, n: N<'t>) -> Variant {
        let name = self.ident(self.field(n, "name").expect("grammar guarantees variant name"));
        let payload = if let Some(rec) = self.field(n, "payload") {
            VariantPayload::Record(
                self.children_of(rec, "field_declaration")
                    .iter()
                    .map(|fd| {
                        (
                            self.ident(self.field(*fd, "name").expect("grammar guarantees field name")),
                            self.lower_type(self.field(*fd, "type").expect("grammar guarantees field type")),
                        )
                    })
                    .collect(),
            )
        } else {
            let tys = self.children_of(n, "type");
            if tys.is_empty() {
                VariantPayload::None
            } else {
                VariantPayload::Tuple(tys.iter().map(|t| self.lower_type(*t)).collect())
            }
        };
        Variant { name, payload }
    }

    fn lower_operator<'t>(&mut self, n: N<'t>) -> Item {
        let level = match n.kind() {
            "function_declaration" => EffectLevel::Function,
            "query_declaration" => EffectLevel::Query,
            _ => EffectLevel::Action,
        };
        let name = self.ident(self.field(n, "name").expect("grammar guarantees operator name"));
        let params = match self.children_of(n, "parameters").first() {
            Some(ps) => self
                .children_of(*ps, "parameter")
                .iter()
                .map(|p| Param {
                    name: self.ident(self.field(*p, "name").expect("grammar guarantees parameter name")),
                    ty: self.lower_type(self.field(*p, "type").expect("grammar guarantees parameter type")),
                })
                .collect(),
            None => vec![],
        };
        let ret = match level {
            EffectLevel::Action => {
                // The grammar fixes the action return type to `set<write_op>`.
                let span = name.span;
                Type::new(
                    TypeKind::Set(Box::new(Type::new(
                        TypeKind::Named { name: Spanned::new("write_op".to_string(), span), args: vec![] },
                        span,
                    ))),
                    span,
                )
            }
            _ => self.lower_type(self.field(n, "return_type").expect("grammar guarantees return type")),
        };
        let decreases = self
            .children_of(n, "decreases_clause")
            .first()
            .map(|d| self.ident(self.field(*d, "measure").expect("grammar guarantees measure")));
        let depth = self.children_of(n, "depth_clause").first().map(|d| {
            self.parse_int_literal(self.field(*d, "bound").expect("grammar guarantees depth bound"))
                .unwrap_or(0) as u64
        });
        let body = self.field(n, "body").map(|b| self.lower_expr(b));
        Item::Operator(OperatorDecl {
            vis: self.visibility(n),
            level,
            recursive: self.has_child(n, "recursive"),
            name,
            type_params: self.type_params(n),
            params,
            ret,
            decreases,
            depth,
            body,
        })
    }

    // ---- types -------------------------------------------------------------

    fn lower_type<'t>(&mut self, n: N<'t>) -> Type {
        let n = self.unwrap(n);
        let span = self.span(n);
        let kind = match n.kind() {
            "primitive_type" => match self.text(n) {
                "bool" => TypeKind::Bool,
                "int" => TypeKind::Int,
                "float" => TypeKind::Float,
                "string" => TypeKind::String,
                "date" => TypeKind::Date,
                other => unreachable!("unknown primitive type `{other}`"),
            },
            "decimal_type" => {
                let nums = self.children_of(n, "int_literal");
                if nums.len() == 2 {
                    let m = self.parse_int_literal(nums[0]).unwrap_or(1) as u32;
                    let s = self.parse_int_literal(nums[1]).unwrap_or(0) as u32;
                    if m < 1 || s > m {
                        self.err(
                            span,
                            format!("invalid decimal precision `decimal({}, {})`: need m >= 1 and 0 <= n <= m", m, s),
                            None,
                        );
                    }
                    TypeKind::Decimal(Some((m, s)))
                } else {
                    TypeKind::Decimal(None)
                }
            }
            "named_type" => {
                let name = self.ident(self.field(n, "name").expect("grammar guarantees type name"));
                let args = match self.field(n, "type_arguments") {
                    Some(ta) => self.children_of(ta, "type").iter().map(|t| self.lower_type(*t)).collect(),
                    None => vec![],
                };
                TypeKind::Named { name, args }
            }
            "key_type" => TypeKind::Key(self.ident(self.field(n, "table").expect("grammar guarantees table name"))),
            "value_type" => TypeKind::Value(self.ident(self.field(n, "table").expect("grammar guarantees table name"))),
            "option_type" => TypeKind::Option(Box::new(self.lower_type(self.field(n, "element").expect("grammar guarantees element type")))),
            "vector_type" => TypeKind::Vector(Box::new(self.lower_type(self.field(n, "element").expect("grammar guarantees element type")))),
            "set_type" => TypeKind::Set(Box::new(self.lower_type(self.field(n, "element").expect("grammar guarantees element type")))),
            "bag_type" => TypeKind::Bag(Box::new(self.lower_type(self.field(n, "element").expect("grammar guarantees element type")))),
            "map_type" => TypeKind::Map(
                Box::new(self.lower_type(self.field(n, "key").expect("grammar guarantees key type"))),
                Box::new(self.lower_type(self.field(n, "value").expect("grammar guarantees value type"))),
            ),
            "table_type" => TypeKind::Table(
                Box::new(self.lower_type(self.field(n, "key").expect("grammar guarantees key type"))),
                Box::new(self.lower_type(self.field(n, "value").expect("grammar guarantees value type"))),
            ),
            "tuple_type" => {
                TypeKind::Tuple(self.children_of(n, "type").iter().map(|t| self.lower_type(*t)).collect())
            }
            "function_type" => {
                // The `parameter` field may also cover the anonymous parens;
                // pick the named `type` child that is not the return type.
                let ret_node = self.field(n, "return_type").expect("grammar guarantees return type");
                let param_node = self
                    .children_of(n, "type")
                    .into_iter()
                    .find(|c| c.id() != ret_node.id())
                    .expect("grammar guarantees function-type parameter");
                TypeKind::Fun(
                    Box::new(self.lower_type(param_node)),
                    Box::new(self.lower_type(ret_node)),
                )
            }
            "record_type" => TypeKind::Record(
                self.children_of(n, "field_declaration")
                    .iter()
                    .map(|fd| {
                        (
                            self.ident(self.field(*fd, "name").expect("grammar guarantees field name")),
                            self.lower_type(self.field(*fd, "type").expect("grammar guarantees field type")),
                        )
                    })
                    .collect(),
            ),
            other => {
                self.err(span, format!("cannot lower type node `{other}`"), None);
                TypeKind::Bool
            }
        };
        Type::new(kind, span)
    }

    // ---- temporal expressions (property bodies) ----------------------------

    fn lower_temporal<'t>(&mut self, n: N<'t>) -> TemporalExpr {
        let n = self.unwrap(n);
        match n.kind() {
            "always_expression" => TemporalExpr::Always(Box::new(
                self.lower_temporal(self.field(n, "operand").expect("grammar guarantees operand")),
            )),
            "eventually_expression" => TemporalExpr::Eventually(Box::new(
                self.lower_temporal(self.field(n, "operand").expect("grammar guarantees operand")),
            )),
            "leads_to_expression" => TemporalExpr::LeadsTo {
                lhs: Box::new(self.lower_temporal(self.field(n, "left").expect("grammar guarantees lhs"))),
                rhs: Box::new(self.lower_temporal(self.field(n, "right").expect("grammar guarantees rhs"))),
            },
            "until_expression" => TemporalExpr::Until {
                lhs: Box::new(self.lower_temporal(self.field(n, "left").expect("grammar guarantees lhs"))),
                rhs: Box::new(self.lower_temporal(self.field(n, "right").expect("grammar guarantees rhs"))),
            },
            _ => TemporalExpr::State(self.lower_temporal_state(n)),
        }
    }

    /// Lower a state predicate inside a `property` body.
    ///
    /// Grammar-quirk repair: in temporal position, the GLR conflict between
    /// extending a quantifier body and reducing it can resolve (depending on
    /// the following token) so that suffix operators bind *outside* the
    /// quantifier — `\A a \in t : a.b >= 0` may parse as
    /// `(\A a \in t : a).b >= 0` instead of `\A a \in t : (a.b >= 0)`. When
    /// the quantifier is the leftmost-deep descendant of the operand along an
    /// operator spine and the operand extends beyond it, push the stolen
    /// suffixes back into the quantifier body. Explicit parentheses stop the
    /// spine (they are user intent).
    fn lower_temporal_state<'t>(&mut self, n: N<'t>) -> Expr {
        let mut spine = Vec::new();
        let mut cur = n;
        let quant = loop {
            let u = self.unwrap(cur);
            match u.kind() {
                "quantifier" => break Some(u),
                "binary_expression" | "comparison_expression" => match self.field(u, "left") {
                    Some(l) => {
                        spine.push(u);
                        cur = l;
                    }
                    None => break None,
                },
                "additive_expression" | "multiplicative_expression" => {
                    spine.push(u);
                    cur = match self.field(u, "left") {
                        Some(l) => l,
                        None => self.named_children(u)[0],
                    };
                }
                "unary_expression" | "cast_expression" | "try_expression" | "primed_expression"
                | "member_expression" => match self.field(u, "operand") {
                    Some(o) => {
                        spine.push(u);
                        cur = o;
                    }
                    None => break None,
                },
                "call_expression" => match self.field(u, "function") {
                    Some(f) => {
                        spine.push(u);
                        cur = f;
                    }
                    None => break None,
                },
                _ => break None,
            }
        };
        let Some(q) = quant else { return self.lower_expr_prime(n) };
        if q.end_byte() >= n.end_byte() {
            return self.lower_expr_prime(n); // parsed correctly already
        }
        // Repair: rebuild the quantifier with the spine's suffixes moved into
        // its body (innermost spine node first).
        let body_node = self.field(q, "body").expect("grammar guarantees quantifier body");
        let mut body = self.lower_expr_prime(body_node);
        for node in spine.iter().rev() {
            body = self.lower_spine_node(*node, body);
        }
        let span = self.span(q);
        let qkind = match self.field(q, "quantifier").map(|x| self.text(x)) {
            Some("\\A") => QuantKind::Forall,
            _ => QuantKind::Exists,
        };
        let gens = self
            .children_of(q, "generator")
            .iter()
            .map(|g| self.lower_generator(*g, true))
            .collect();
        Expr::new(ExprKind::Quantifier { kind: qkind, gens, body: Box::new(body) }, span)
    }

    /// Rebuild one operator-spine node with its leftmost operand replaced by
    /// the already-lowered `inner` (see [`Lower::lower_temporal_state`]).
    fn lower_spine_node<'t>(&mut self, n: N<'t>, inner: Expr) -> Expr {
        let span = self.span(n);
        let kind = match n.kind() {
            "binary_expression" | "comparison_expression" | "additive_expression"
            | "multiplicative_expression" => {
                let Some(op_node) = self.field(n, "operator") else { return inner };
                ExprKind::BinOp {
                    op: self.binop_kind(self.text(op_node)),
                    lhs: Box::new(inner),
                    rhs: Box::new(self.lower_expr_prime(self.field(n, "right").expect("grammar guarantees rhs"))),
                }
            }
            "unary_expression" => {
                let op = match self.field(n, "operator").map(|o| self.text(o)) {
                    Some("-") => UnOpKind::Neg,
                    _ => UnOpKind::Not,
                };
                ExprKind::UnOp { op, operand: Box::new(inner) }
            }
            "cast_expression" => ExprKind::Cast {
                expr: Box::new(inner),
                ty: self.lower_type(self.field(n, "type").expect("grammar guarantees cast type")),
            },
            "try_expression" => ExprKind::Try(Box::new(inner)),
            "primed_expression" => ExprKind::Primed(Box::new(inner)),
            "member_expression" => {
                let member = self.field(n, "member").expect("grammar guarantees member");
                if member.kind() == "int_literal" {
                    let idx = self.parse_int_literal(member).unwrap_or(0);
                    ExprKind::TupleProj { base: Box::new(inner), index: idx as u32 }
                } else {
                    ExprKind::Field { base: Box::new(inner), name: self.ident(member) }
                }
            }
            "call_expression" => {
                let args_node = self.field(n, "arguments").expect("grammar guarantees arguments");
                let args: Vec<Arg> = self
                    .children_of(args_node, "argument")
                    .iter()
                    .map(|a| Arg {
                        name: self.field(*a, "name").map(|id| self.ident(id)),
                        value: self.lower_expr_prime(self.field(*a, "value").expect("grammar guarantees argument value")),
                    })
                    .collect();
                let type_args = self
                    .field(n, "type_arguments")
                    .map(|ta| self.children_of(ta, "type").iter().map(|t| self.lower_type(*t)).collect::<Vec<_>>());
                // Rebuild the call shape the stolen suffix would have had
                // inside the else branch: `f(args)` → Call, `r.m(args)` →
                // MethodCall, anything else → App.
                let func_node = self.field(n, "function").expect("grammar guarantees callee");
                match (func_node.kind(), inner.kind) {
                    ("ident", ExprKind::Var(name)) => ExprKind::Call(Call { name, type_args, args }),
                    ("member_expression", ExprKind::Field { base, name }) => {
                        ExprKind::MethodCall { recv: base, name, args }
                    }
                    (_, kind) => ExprKind::App { func: Box::new(Expr::new(kind, inner.span)), args },
                }
            }
            _ => return inner,
        };
        Expr::new(kind, span)
    }

    // ---- expressions -------------------------------------------------------

    /// Lower an expression inside a `property` body, where the postfix prime
    /// operator is legal.
    fn lower_expr_prime<'t>(&mut self, n: N<'t>) -> Expr {
        if n.kind() == "primed_expression" {
            let operand = self.field(n, "operand").expect("grammar guarantees prime operand");
            let span = self.span(n);
            return Expr::new(ExprKind::Primed(Box::new(self.lower_expr_prime(operand))), span);
        }
        self.lower_expr_inner(n, true)
    }

    fn lower_expr<'t>(&mut self, n: N<'t>) -> Expr {
        self.lower_expr_inner(n, false)
    }

    fn lower_expr_inner<'t>(&mut self, n: N<'t>, in_property: bool) -> Expr {
        let n = self.unwrap(n);
        // Grammar-quirk repair (A.3: the else branch of an `if` is a full
        // expression that eats to the right): postfix suffixes (`(args)`,
        // `.field`, `?`, `'`) following an `if`-expression may bind *outside*
        // the `if` depending on the following token; push them back into the
        // else branch. Must run before the prime intercept so that a stolen
        // prime lands on the else branch rather than the whole `if`.
        if let Some(repaired) = self.try_repair_if_suffix(n, in_property) {
            return repaired;
        }
        // Inside `property` bodies the prime operator is legal; route any
        // primed node (however deeply nested) through the prime-aware path.
        if in_property && n.kind() == "primed_expression" {
            return self.lower_expr_prime(n);
        }
        let span = self.span(n);
        let kind = match n.kind() {
            "binary_expression" => self.lower_binop(n, in_property),
            "comparison_expression" => self.lower_binop(n, in_property),
            "additive_expression" | "multiplicative_expression" => {
                if self.field(n, "operator").is_some() {
                    self.lower_binop(n, in_property)
                } else {
                    // Unary chain wrapper: descend to the single child.
                    return self.lower_expr_inner(self.named_children(n)[0], in_property);
                }
            }
            "unary_expression" => {
                let op = match self.field(n, "operator").map(|o| self.text(o)) {
                    Some("~") => UnOpKind::Not,
                    Some("-") => UnOpKind::Neg,
                    _ => UnOpKind::Not,
                };
                ExprKind::UnOp {
                    op,
                    operand: Box::new(self.lower_expr_ctx(self.field(n, "operand").expect("grammar guarantees operand"), in_property)),
                }
            }
            "cast_expression" => ExprKind::Cast {
                expr: Box::new(self.lower_expr_ctx(self.field(n, "operand").expect("grammar guarantees operand"), in_property)),
                ty: self.lower_type(self.field(n, "type").expect("grammar guarantees cast type")),
            },
            "member_expression" => {
                let base = self.lower_expr_ctx(self.field(n, "operand").expect("grammar guarantees operand"), in_property);
                let member = self.field(n, "member").expect("grammar guarantees member");
                if member.kind() == "int_literal" {
                    let idx = self.parse_int_literal(member).unwrap_or(0);
                    if idx > u32::MAX as i128 {
                        self.err(self.span(member), format!("tuple index `{}` is too large", idx), None);
                    }
                    ExprKind::TupleProj { base: Box::new(base), index: idx as u32 }
                } else {
                    ExprKind::Field { base: Box::new(base), name: self.ident(member) }
                }
            }
            "call_expression" => self.lower_call(n, in_property),
            "try_expression" => ExprKind::Try(Box::new(
                self.lower_expr_ctx(self.field(n, "operand").expect("grammar guarantees operand"), in_property),
            )),
            "primed_expression" => {
                self.err(
                    span,
                    "the prime operator `'` is only allowed inside `property` bodies",
                    None,
                );
                // Recovery: lower the operand as a plain expression.
                return self.lower_expr(self.field(n, "operand").expect("grammar guarantees prime operand"));
            }
            "parenthesized_expression" => {
                return self.lower_expr_ctx(self.named_children(n)[0], in_property);
            }
            "tuple_literal" => ExprKind::Tuple(
                self.named_children(n).iter().map(|c| self.lower_expr_ctx(*c, in_property)).collect(),
            ),
            "vector_literal" => ExprKind::Vector(
                self.named_children(n).iter().map(|c| self.lower_expr_ctx(*c, in_property)).collect(),
            ),
            "block" => {
                let lets = self
                    .children_of(n, "let_binding")
                    .iter()
                    .map(|lb| {
                        let pat = self.lower_pattern(self.named_children(*lb)[0]);
                        let ty = self.field(*lb, "type").map(|t| self.lower_type(t));
                        let value = self.lower_expr_ctx(self.field(*lb, "value").expect("grammar guarantees let value"), in_property);
                        LetStmt { pat, ty, value }
                    })
                    .collect();
                let tail = self.lower_expr_ctx(self.field(n, "body").expect("grammar guarantees block body"), in_property);
                ExprKind::Block { lets, tail: Box::new(tail) }
            }
            "if_expression" => ExprKind::If {
                cond: Box::new(self.lower_expr_ctx(self.field(n, "condition").expect("grammar guarantees condition"), in_property)),
                then_br: Box::new(self.lower_expr_ctx(self.field(n, "consequence").expect("grammar guarantees consequence"), in_property)),
                else_br: Box::new(self.lower_expr_ctx(self.field(n, "alternative").expect("grammar guarantees alternative"), in_property)),
            },
            "match_expression" => ExprKind::Match {
                scrutinee: Box::new(self.lower_expr_ctx(self.field(n, "scrutinee").expect("grammar guarantees scrutinee"), in_property)),
                arms: self
                    .children_of(n, "match_arm")
                    .iter()
                    .map(|arm| MatchArm {
                        pat: self.lower_pattern(self.named_children(*arm)[0]),
                        body: self.lower_expr_ctx(self.field(*arm, "body").expect("grammar guarantees arm body"), in_property),
                    })
                    .collect(),
            },
            "set_form" => self.lower_set_form(n, in_property),
            "bag_form" => {
                let map = self.children_of(n, "map_form");
                if let Some(mf) = map.first() {
                    let (elem, gens) = self.lower_map_form(*mf, in_property);
                    ExprKind::BagMap { elem: Box::new(elem), gens }
                } else {
                    ExprKind::BagLiteral(
                        self.named_children(n).iter().map(|c| self.lower_expr_ctx(*c, in_property)).collect(),
                    )
                }
            }
            "map_literal" => ExprKind::MapLit(
                self.children_of(n, "map_entry")
                    .iter()
                    .map(|e| {
                        (
                            self.lower_expr_ctx(self.field(*e, "key").expect("grammar guarantees entry key"), in_property),
                            self.lower_expr_ctx(self.field(*e, "value").expect("grammar guarantees entry value"), in_property),
                        )
                    })
                    .collect(),
            ),
            "record_literal" => ExprKind::RecordLit { fields: self.record_fields(n, in_property) },
            "record_update" => ExprKind::RecordUpd {
                base: Box::new(self.lower_expr_ctx(self.field(n, "base").expect("grammar guarantees update base"), in_property)),
                fields: self.record_fields(n, in_property),
            },
            "quantifier" => {
                let qkind = match self.field(n, "quantifier").map(|q| self.text(q)) {
                    Some("\\A") => QuantKind::Forall,
                    _ => QuantKind::Exists,
                };
                let gens = self
                    .children_of(n, "generator")
                    .iter()
                    .map(|g| self.lower_generator(*g, in_property))
                    .collect();
                let body = self.lower_expr_ctx(self.field(n, "body").expect("grammar guarantees quantifier body"), in_property);
                ExprKind::Quantifier { kind: qkind, gens, body: Box::new(body) }
            }
            "option_literal" => match self.field(n, "value") {
                Some(v) => ExprKind::OptionSome(Box::new(self.lower_expr_ctx(v, in_property))),
                None => ExprKind::OptionNone,
            },
            "lambda" => self.lower_lambda(n, in_property),
            "int_literal" => ExprKind::Lit(Literal::Int(self.parse_int_literal(n).unwrap_or(0) as i64)),
            "float_literal" => ExprKind::Lit(Literal::Float(self.parse_float_literal(n))),
            "boolean_literal" => ExprKind::Lit(Literal::Bool(self.text(n) == "true")),
            "string_literal" => return self.lower_string_expr(n),
            "date_literal" => return self.lower_date_literal(n),
            "decimal_literal" => return self.lower_decimal_literal(n),
            "ident" => ExprKind::Var(self.ident(n)),
            other => {
                self.err(span, format!("cannot lower expression node `{other}`"), None);
                ExprKind::Lit(Literal::Int(0))
            }
        };
        Expr::new(kind, span)
    }

    /// Lower an expression node, dispatching to the prime-aware path inside
    /// `property` bodies.
    fn lower_expr_ctx<'t>(&mut self, n: N<'t>, in_property: bool) -> Expr {
        if in_property {
            self.lower_expr_prime(n)
        } else {
            self.lower_expr_inner(n, false)
        }
    }

    fn lower_binop<'t>(&mut self, n: N<'t>, in_property: bool) -> ExprKind {
        let op_text = self.field(n, "operator").map(|o| self.text(o)).unwrap_or("=");
        let op = self.binop_kind(op_text);
        ExprKind::BinOp {
            op,
            lhs: Box::new(self.lower_expr_ctx(self.field(n, "left").expect("grammar guarantees lhs"), in_property)),
            rhs: Box::new(self.lower_expr_ctx(self.field(n, "right").expect("grammar guarantees rhs"), in_property)),
        }
    }

    /// Map an operator token to its [`BinOpKind`].
    fn binop_kind(&self, op_text: &str) -> BinOpKind {
        match op_text {
            "=>" => BinOpKind::Impl,
            "\\/" => BinOpKind::Or,
            "/\\" => BinOpKind::And,
            "+" => BinOpKind::Add,
            "-" => BinOpKind::Sub,
            "*" => BinOpKind::Mul,
            "/" => BinOpKind::Div,
            "%" => BinOpKind::Mod,
            "=" => BinOpKind::Eq,
            "/=" => BinOpKind::Ne,
            "<" => BinOpKind::Lt,
            ">" => BinOpKind::Gt,
            "<=" => BinOpKind::Le,
            ">=" => BinOpKind::Ge,
            "\\in" => BinOpKind::In,
            "\\subseteq" => BinOpKind::SubsetEq,
            "\\cup" => BinOpKind::Cup,
            "\\cap" => BinOpKind::Cap,
            "\\" => BinOpKind::Diff,
            "\\X" => BinOpKind::Cartesian,
            other => unreachable!("unknown binary operator `{other}`"),
        }
    }

    /// Detect and repair the `if`-suffix mis-parse (see
    /// [`Lower::lower_expr_inner`]): when an `if_expression` is the
    /// leftmost-deep descendant of `n` along a postfix spine (`call`/
    /// `member`/`try`/`primed`) and `n` extends beyond the `if`, the suffixes
    /// belong to the else branch. Returns the rebuilt `If` expression, or
    /// `None` when `n` needs no repair. Parentheses and non-postfix operators
    /// stop the spine (binary operators and `as` bind inside the else branch
    /// correctly, so they never steal).
    fn try_repair_if_suffix<'t>(&mut self, n: N<'t>, in_property: bool) -> Option<Expr> {
        let mut spine = Vec::new();
        let mut cur = n;
        let if_node = loop {
            match cur.kind() {
                "if_expression" => break Some(cur),
                "call_expression" => {
                    spine.push(cur);
                    cur = self.field(cur, "function")?;
                }
                "member_expression" | "try_expression" | "primed_expression" => {
                    spine.push(cur);
                    cur = self.field(cur, "operand")?;
                }
                _ => break None,
            }
        };
        let ifn = if_node?;
        if ifn.end_byte() >= n.end_byte() {
            return None; // the else branch already ate everything
        }
        // Rebuild: else' = spine applied to the else branch (innermost first).
        let alt = self.field(ifn, "alternative").expect("grammar guarantees else branch");
        let mut alt_expr = self.lower_expr_ctx(alt, in_property);
        for node in spine.iter().rev() {
            alt_expr = self.lower_spine_node(*node, alt_expr);
        }
        let cond = self.lower_expr_ctx(self.field(ifn, "condition").expect("grammar guarantees condition"), in_property);
        let then_br = self.lower_expr_ctx(self.field(ifn, "consequence").expect("grammar guarantees consequence"), in_property);
        Some(Expr::new(
            ExprKind::If { cond: Box::new(cond), then_br: Box::new(then_br), else_br: Box::new(alt_expr) },
            self.span(n),
        ))
    }

    fn lower_call<'t>(&mut self, n: N<'t>, in_property: bool) -> ExprKind {
        let func = self.field(n, "function").expect("grammar guarantees callee");
        let args_node = self.field(n, "arguments").expect("grammar guarantees arguments");
        let args: Vec<Arg> = self
            .children_of(args_node, "argument")
            .iter()
            .map(|a| Arg {
                name: self.field(*a, "name").map(|id| self.ident(id)),
                value: self.lower_expr_ctx(self.field(*a, "value").expect("grammar guarantees argument value"), in_property),
            })
            .collect();
        let type_args = self
            .field(n, "type_arguments")
            .map(|ta| self.children_of(ta, "type").iter().map(|t| self.lower_type(*t)).collect::<Vec<_>>());
        if func.kind() == "member_expression" {
            // Method call `recv.name(args)`.
            if type_args.is_some() {
                self.err(
                    self.span(n),
                    "type arguments are not allowed on method calls",
                    None,
                );
            }
            let recv = self.lower_expr_ctx(self.field(func, "operand").expect("grammar guarantees receiver"), in_property);
            let member = self.field(func, "member").expect("grammar guarantees member");
            let name = self.ident(member);
            return ExprKind::MethodCall { recv: Box::new(recv), name, args };
        }
        if func.kind() == "ident" {
            return ExprKind::Call(Call { name: self.ident(func), type_args, args });
        }
        ExprKind::App { func: Box::new(self.lower_expr_ctx(func, in_property)), args }
    }

    fn lower_set_form<'t>(&mut self, n: N<'t>, in_property: bool) -> ExprKind {
        if let Some(ff) = self.children_of(n, "filter_form").first() {
            let pat = self.lower_pattern(self.named_children(*ff)[0]);
            let source = self.lower_expr_ctx(self.field(*ff, "collection").expect("grammar guarantees filter source"), in_property);
            let pred = self.lower_expr_ctx(self.field(*ff, "predicate").expect("grammar guarantees filter predicate"), in_property);
            return ExprKind::SetFilter { pat, source: Box::new(source), pred: Box::new(pred) };
        }
        if let Some(mf) = self.children_of(n, "map_form").first() {
            let (elem, gens) = self.lower_map_form(*mf, in_property);
            return ExprKind::SetMap { elem: Box::new(elem), gens };
        }
        ExprKind::SetLiteral(
            self.named_children(n).iter().map(|c| self.lower_expr_ctx(*c, in_property)).collect(),
        )
    }

    fn lower_map_form<'t>(&mut self, n: N<'t>, in_property: bool) -> (Expr, Vec<Generator>) {
        let elem = self.lower_expr_ctx(self.field(n, "key").expect("grammar guarantees map-form element"), in_property);
        let gens = self
            .children_of(n, "generator")
            .iter()
            .map(|g| self.lower_generator(*g, in_property))
            .collect();
        (elem, gens)
    }

    fn lower_generator<'t>(&mut self, n: N<'t>, in_property: bool) -> Generator {
        Generator {
            pat: self.lower_pattern(self.named_children(n)[0]),
            source: self.lower_expr_ctx(self.field(n, "collection").expect("grammar guarantees generator source"), in_property),
        }
    }

    fn record_fields<'t>(&mut self, n: N<'t>, in_property: bool) -> Vec<FieldInit> {
        self.children_of(n, "record_field")
            .iter()
            .map(|f| FieldInit {
                name: self.ident(self.field(*f, "name").expect("grammar guarantees field name")),
                value: self.lower_expr_ctx(self.field(*f, "value").expect("grammar guarantees field value"), in_property),
            })
            .collect()
    }

    fn lower_lambda<'t>(&mut self, n: N<'t>, in_property: bool) -> ExprKind {
        let captures = match self.children_of(n, "capture_list").first() {
            Some(cl) => self.children_of(*cl, "ident").iter().map(|i| self.ident(*i)).collect(),
            None => vec![],
        };
        let params = self
            .children_of(n, "lambda_parameter")
            .iter()
            .map(|p| LambdaParam {
                pat: self.lower_pattern(self.named_children(*p)[0]),
                ty: self.field(*p, "type").map(|t| self.lower_type(t)),
            })
            .collect();
        let ret = self.field(n, "return_type").map(|t| self.lower_type(t));
        let body = self.lower_expr_ctx(self.field(n, "body").expect("grammar guarantees lambda body"), in_property);
        ExprKind::Lambda(Lambda { captures, params, ret, body: Box::new(body) })
    }

    // ---- patterns ----------------------------------------------------------

    fn lower_pattern<'t>(&mut self, n: N<'t>) -> Pattern {
        let n = self.unwrap(n);
        let span = self.span(n);
        let kind = match n.kind() {
            "wildcard_pattern" => PatternKind::Wildcard,
            "ident" => PatternKind::Bind(self.ident(n)),
            "int_literal" => PatternKind::Lit(PatLit::Int(self.parse_int_literal(n).unwrap_or(0) as i64)),
            "boolean_literal" => PatternKind::Lit(PatLit::Bool(self.text(n) == "true")),
            "string_literal" => {
                PatternKind::Lit(PatLit::Str(self.eval_string_parts(n).0))
            }
            "option_pattern" => match self.named_children(n).first() {
                Some(inner) => PatternKind::Some(Box::new(self.lower_pattern(*inner))),
                None => PatternKind::None,
            },
            "variant_pattern" => PatternKind::Variant {
                name: self.ident(self.field(n, "name").expect("grammar guarantees variant name")),
                args: self
                    .children_of(n, "pattern")
                    .iter()
                    .map(|p| self.lower_pattern(*p))
                    .collect(),
            },
            "tuple_pattern" => PatternKind::Tuple(
                self.children_of(n, "pattern").iter().map(|p| self.lower_pattern(*p)).collect(),
            ),
            "record_pattern" => PatternKind::Record(
                self.children_of(n, "ident").iter().map(|i| self.ident(*i)).collect(),
            ),
            "vector_pattern" => {
                let pats = self.children_of(n, "pattern");
                match pats.len() {
                    0 => PatternKind::ConsNil,
                    2 => PatternKind::Cons {
                        head: Box::new(self.lower_pattern(pats[0])),
                        tail: Box::new(self.lower_pattern(pats[1])),
                    },
                    _ => {
                        self.err(span, "malformed vector pattern", None);
                        PatternKind::Wildcard
                    }
                }
            }
            other => {
                self.err(span, format!("cannot lower pattern node `{other}`"), None);
                PatternKind::Wildcard
            }
        };
        Pattern::new(kind, span)
    }

    // ---- literals ----------------------------------------------------------

    /// Parse an int literal: `0`, decimal with `_` separators, or `0x` hex.
    /// Errors (overflow) are reported and yield `None`.
    fn parse_int_literal<'t>(&mut self, n: N<'t>) -> Option<i128> {
        let raw = self.text(n);
        let clean: String = raw.chars().filter(|c| *c != '_').collect();
        let parsed = if let Some(hex) = clean.strip_prefix("0x") {
            i128::from_str_radix(hex, 16)
        } else {
            clean.parse::<i128>()
        };
        match parsed {
            Ok(v) if v <= i64::MAX as i128 => Some(v),
            Ok(_) => {
                self.err(self.span(n), format!("integer literal `{raw}` overflows `int` (i64)"), None);
                None
            }
            Err(_) => {
                self.err(self.span(n), format!("malformed integer literal `{raw}`"), None);
                None
            }
        }
    }

    fn parse_float_literal<'t>(&mut self, n: N<'t>) -> f64 {
        let raw = self.text(n);
        let clean: String = raw.chars().filter(|c| *c != '_').collect();
        match clean.parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                self.err(self.span(n), format!("malformed float literal `{raw}`"), None);
                0.0
            }
        }
    }

    /// Evaluate a string literal into (text, interpolation parts). The text is
    /// the concatenation of literal fragments; interpolations are returned as
    /// ordered parts with their lowered expressions. Returns `(full_text,
    /// parts)` where `parts` is empty for plain strings.
    fn eval_string_parts<'t>(&mut self, n: N<'t>) -> (String, Vec<StrPart>) {
        let mut text = String::new();
        let mut parts: Vec<StrPart> = Vec::new();
        for c in self.all_children(n) {
            match c.kind() {
                "string_content" => {
                    let t = self.text(c);
                    text.push_str(t);
                    push_lit(&mut parts, t);
                }
                "escape_sequence" => {
                    let s = self.eval_escape(c);
                    text.push_str(&s);
                    push_lit(&mut parts, &s);
                }
                "interpolation" => {
                    let inner = self.named_children(c)[0];
                    parts.push(StrPart::Interp(self.lower_expr(inner)));
                }
                _ => {}
            }
        }
        (text, parts)
    }

    fn eval_escape<'t>(&mut self, n: N<'t>) -> String {
        match self.text(n) {
            "\\n" => "\n".to_string(),
            "\\t" => "\t".to_string(),
            "\\\\" => "\\".to_string(),
            "\\\"" => "\"".to_string(),
            raw if raw.starts_with("\\u{") && raw.ends_with('}') => {
                let hex = &raw[3..raw.len() - 1];
                match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => ch.to_string(),
                    None => {
                        self.err(self.span(n), format!("invalid unicode escape `{raw}`"), None);
                        String::new()
                    }
                }
            }
            raw => {
                self.err(self.span(n), format!("unknown escape `{raw}`"), None);
                String::new()
            }
        }
    }

    fn lower_string_expr<'t>(&mut self, n: N<'t>) -> Expr {
        let span = self.span(n);
        let (text, parts) = self.eval_string_parts(n);
        if parts.iter().all(|p| matches!(p, StrPart::Lit(_))) {
            Expr::new(ExprKind::Lit(Literal::Str(text)), span)
        } else {
            Expr::new(ExprKind::StrInterp(parts), span)
        }
    }

    fn lower_date_literal<'t>(&mut self, n: N<'t>) -> Expr {
        let span = self.span(n);
        let str_node = self.field(n, "value").expect("grammar guarantees date string");
        // A date literal must be a plain string (no interpolation/escapes).
        let has_interp = self.named_children(str_node).iter().any(|c| c.kind() != "string_content");
        if has_interp {
            self.err(self.span(str_node), "a date literal must be a plain `YYYY-MM-DD` string", None);
            return Expr::new(ExprKind::Lit(Literal::Date { year: 0, month: 1, day: 1 }), span);
        }
        let raw = self.text(str_node).trim_matches('"');
        let parts: Vec<&str> = raw.split('-').collect();
        let parsed = match parts.as_slice() {
            [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2 => {
                match (y.parse::<i32>(), m.parse::<u8>(), d.parse::<u8>()) {
                    (Ok(year), Ok(month), Ok(day)) => Some(Literal::Date { year, month, day }),
                    _ => None,
                }
            }
            _ => None,
        };
        match parsed {
            Some(lit) => Expr::new(ExprKind::Lit(lit), span),
            None => {
                self.err(
                    self.span(str_node),
                    format!("malformed date literal `date \"{raw}\"`: expected `YYYY-MM-DD`"),
                    None,
                );
                Expr::new(ExprKind::Lit(Literal::Date { year: 0, month: 1, day: 1 }), span)
            }
        }
    }

    fn lower_decimal_literal<'t>(&mut self, n: N<'t>) -> Expr {
        let span = self.span(n);
        let value = self.field(n, "value").expect("grammar guarantees decimal value");
        let repr: String = self.text(value).chars().filter(|c| *c != '_').collect();
        // The two precision ints (if any) are the non-value int_literal children.
        let precision = {
            let nums = self.children_of(n, "int_literal");
            let nums: Vec<N> = nums.into_iter().filter(|c| c.id() != value.id()).collect();
            if nums.len() == 2 {
                let m = self.parse_int_literal(nums[0]).unwrap_or(1) as u32;
                let s = self.parse_int_literal(nums[1]).unwrap_or(0) as u32;
                if m < 1 || s > m {
                    self.err(
                        span,
                        format!("invalid decimal precision `decimal({}, {})`: need m >= 1 and 0 <= n <= m", m, s),
                        None,
                    );
                }
                Some((m, s))
            } else {
                None
            }
        };
        Expr::new(ExprKind::Lit(Literal::Decimal { repr, precision }), span)
    }
}

/// Merge a literal fragment into the trailing `StrPart::Lit`, if any.
fn push_lit(parts: &mut Vec<StrPart>, s: &str) {
    if s.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(StrPart::Lit(acc)) => acc.push_str(s),
        _ => parts.push(StrPart::Lit(s.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Module {
        match parse_module(src) {
            Ok(m) => m,
            Err(bag) => panic!("parse failed:\n{}", bag.render()),
        }
    }

    fn parse_err(src: &str) -> DiagBag {
        match parse_module(src) {
            Ok(_) => panic!("expected parse errors, got Ok"),
            Err(bag) => bag,
        }
    }

    fn msgs(bag: &DiagBag) -> Vec<String> {
        bag.errors().iter().map(|e| e.message().to_string()).collect()
    }

    /// Extract the single operator's body expression.
    fn body(m: &Module) -> &Expr {
        m.items
            .iter()
            .find_map(|it| match it {
                Item::Operator(o) => o.body.as_ref(),
                _ => None,
            })
            .expect("operator with body")
    }

    fn tail(m: &Module) -> &Expr {
        let ExprKind::Block { tail, .. } = &body(m).kind else { panic!("body is a block") };
        tail
    }

    // ---- module structure ---------------------------------------------------

    #[test]
    fn module_header_and_items() {
        let m = parse_ok(
            "module shop;\nuse util;\nuse a::b::c as d;\nconst answer: int == 42;\n",
        );
        assert_eq!(m.name.node, "shop");
        assert_eq!(m.items.len(), 3);
        let Item::Use(u) = &m.items[0] else { panic!() };
        assert_eq!(u.path.len(), 1);
        let Item::Use(u2) = &m.items[1] else { panic!() };
        assert_eq!(u2.path.len(), 3);
        assert_eq!(u2.alias.as_ref().unwrap().node, "d");
        let Item::Const(c) = &m.items[2] else { panic!() };
        assert_eq!(c.name.node, "answer");
        assert_eq!(c.vis, Visibility::Private);
        assert!(matches!(c.ty.kind, TypeKind::Int));
        assert!(matches!(c.value.kind, ExprKind::Lit(Literal::Int(42))));
    }

    #[test]
    fn table_index_and_visibility() {
        let m = parse_ok(
            "module m;\npublic table users { id: int, name: string } primary key {id} foreign key {id} references other\nindex by_name on users(name)\n",
        );
        let Item::Table(t) = &m.items[0] else { panic!() };
        assert_eq!(t.vis, Visibility::Public);
        assert_eq!(t.fields.len(), 2);
        assert_eq!(t.pk[0].node, "id");
        assert_eq!(t.fks[0].references.node, "other");
        let Item::Index(i) = &m.items[1] else { panic!() };
        assert_eq!(i.table.node, "users");
        assert_eq!(i.cols[0].node, "name");
    }

    #[test]
    fn enum_variants_three_shapes() {
        let m = parse_ok("module m;\nenum e<T> { a, b(int, T), c { x: int } }\n");
        let Item::Enum(e) = &m.items[0] else { panic!() };
        assert_eq!(e.params.len(), 1);
        assert!(matches!(e.variants[0].payload, VariantPayload::None));
        let VariantPayload::Tuple(t) = &e.variants[1].payload else { panic!() };
        assert_eq!(t.len(), 2);
        let VariantPayload::Record(fs) = &e.variants[2].payload else { panic!() };
        assert_eq!(fs[0].0.node, "x");
    }

    #[test]
    fn operator_shapes() {
        let m = parse_ok(
            "module m;\nfunction recursive f(n: int) -> int decreases n with depth 4 == { n }\nquery g() -> int == { 1 }\naction h(x: int) -> set<write_op> == { set {} }\n",
        );
        let Item::Operator(f) = &m.items[0] else { panic!() };
        assert!(f.recursive);
        assert_eq!(f.level, EffectLevel::Function);
        assert_eq!(f.decreases.as_ref().unwrap().node, "n");
        assert_eq!(f.depth, Some(4));
        let Item::Operator(h) = &m.items[2] else { panic!() };
        assert_eq!(h.level, EffectLevel::Action);
        // Action return type is the fixed `set<write_op>`.
        let TypeKind::Set(inner) = &h.ret.kind else { panic!() };
        let TypeKind::Named { name, .. } = &inner.kind else { panic!() };
        assert_eq!(name.node, "write_op");
    }

    #[test]
    fn external_function_has_no_body() {
        let m = parse_ok("module m;\nfunction ext(x: int) -> int\n");
        let Item::Operator(o) = &m.items[0] else { panic!() };
        assert!(o.body.is_none());
    }

    #[test]
    fn test_property_fairness_items() {
        let m = parse_ok(
            "module m;\ntest t1 { fixture users == [record { id: 1 }]; expect q() == 1; }\nproperty p == [](q() = 1)\nfairness strong == a1, a2\ninvariant inv on users == size(users) >= 0\n",
        );
        let Item::Test(t) = &m.items[0] else { panic!() };
        assert_eq!(t.stmts.len(), 2);
        assert!(matches!(t.stmts[0], TestStmt::Fixture { .. }));
        assert!(matches!(t.stmts[1], TestStmt::Expect { .. }));
        let Item::Property(p) = &m.items[1] else { panic!() };
        assert!(matches!(p.body, TemporalExpr::Always(_)));
        let Item::Fairness(f) = &m.items[2] else { panic!() };
        assert_eq!(f.kind, FairnessKind::Strong);
        assert_eq!(f.actions.len(), 2);
        let Item::Invariant(i) = &m.items[3] else { panic!() };
        assert_eq!(i.table.node, "users");
    }

    // ---- types --------------------------------------------------------------

    #[test]
    fn type_forms() {
        let m = parse_ok(
            "module m;\ntype a == decimal(10, 2);\ntype b == key t;\ntype c == value t;\ntype d == option<vector<set<bag<int>>>>;\ntype e == map<string, int>;\ntype f == (int, string, bool);\ntype g == int -> int -> int;\ntype h == { x: int, y: string };\ntype i<T> == vector<T>;\n",
        );
        let ty = |i: usize| match &m.items[i] {
            Item::TypeAlias(t) => &t.ty.kind,
            _ => panic!(),
        };
        assert!(matches!(ty(0), TypeKind::Decimal(Some((10, 2)))));
        assert!(matches!(ty(1), TypeKind::Key(_)));
        assert!(matches!(ty(2), TypeKind::Value(_)));
        assert!(matches!(ty(3), TypeKind::Option(_)));
        assert!(matches!(ty(4), TypeKind::Map(_, _)));
        let TypeKind::Tuple(ts) = ty(5) else { panic!() };
        assert_eq!(ts.len(), 3);
        // Function types are right-associative: int -> (int -> int).
        let TypeKind::Fun(_, r) = ty(6) else { panic!() };
        assert!(matches!(r.kind, TypeKind::Fun(_, _)));
        let TypeKind::Record(fs) = ty(7) else { panic!() };
        assert_eq!(fs.len(), 2);
        let Item::TypeAlias(t) = &m.items[8] else { panic!() };
        assert_eq!(t.params[0].node, "T");
    }

    #[test]
    fn invalid_decimal_precision_errors() {
        let bag = parse_err("module m;\ntype a == decimal(2, 5);\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("invalid decimal precision")));
    }

    // ---- expressions: operators ---------------------------------------------

    #[test]
    fn binary_operator_mapping_and_precedence() {
        let m = parse_ok("module m;\nfunction f() -> bool == { a + b * c => d \\/ e /\\ f }\n");
        // `=>` is lowest: root is Impl.
        let ExprKind::BinOp { op: BinOpKind::Impl, lhs, rhs } = &tail(&m).kind else {
            panic!("root not Impl: {:?}", tail(&m).kind)
        };
        // lhs = a + (b*c)
        let ExprKind::BinOp { op: BinOpKind::Add, rhs: mul, .. } = &lhs.kind else { panic!("lhs not Add") };
        assert!(matches!(mul.kind, ExprKind::BinOp { op: BinOpKind::Mul, .. }));
        // rhs = d \/ (e /\ f)  (/\ binds tighter than \/)
        let ExprKind::BinOp { op: BinOpKind::Or, rhs: and, .. } = &rhs.kind else { panic!("rhs not Or") };
        assert!(matches!(and.kind, ExprKind::BinOp { op: BinOpKind::And, .. }));
    }

    #[test]
    fn comparison_operators_map() {
        for (src_op, want) in [
            ("=", BinOpKind::Eq),
            ("/=", BinOpKind::Ne),
            ("<", BinOpKind::Lt),
            (">", BinOpKind::Gt),
            ("<=", BinOpKind::Le),
            (">=", BinOpKind::Ge),
            ("\\in", BinOpKind::In),
            ("\\subseteq", BinOpKind::SubsetEq),
        ] {
            let m = parse_ok(&format!("module m;\nfunction f() -> bool == {{ a {src_op} b }}\n"));
            let ExprKind::BinOp { op, .. } = &tail(&m).kind else { panic!("{src_op} not a binop") };
            assert_eq!(*op, want, "{src_op}");
        }
    }

    #[test]
    fn comparison_not_chainable_is_syntax_error() {
        let bag = parse_err("module m;\nfunction f() -> bool == { a = b = c }\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("syntax error")));
    }

    #[test]
    fn additive_in_comparison_operand() {
        // `a + b = c` — the additive chain lives under the comparison.
        let m = parse_ok("module m;\nfunction f() -> bool == { a + b = c }\n");
        let ExprKind::BinOp { op: BinOpKind::Eq, lhs, .. } = &tail(&m).kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::BinOp { op: BinOpKind::Add, .. }));
    }

    #[test]
    fn unary_cast_try_prime() {
        let m = parse_ok("module m;\nfunction f() -> int == { ~x + -y }\n");
        let ExprKind::BinOp { lhs, rhs, .. } = &tail(&m).kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::UnOp { op: UnOpKind::Not, .. }));
        assert!(matches!(rhs.kind, ExprKind::UnOp { op: UnOpKind::Neg, .. }));

        let m = parse_ok("module m;\nfunction f() -> float == { x as float }\n");
        let ExprKind::Cast { ty, .. } = &tail(&m).kind else { panic!() };
        assert!(matches!(ty.kind, TypeKind::Float));

        let m = parse_ok("module m;\nfunction f() -> int == { g(x)? }\n");
        let ExprKind::Try(inner) = &tail(&m).kind else { panic!() };
        assert!(matches!(inner.kind, ExprKind::Call(_)));
    }

    #[test]
    fn prime_outside_property_is_error() {
        let bag = parse_err("module m;\nfunction f() -> int == { x' }\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("prime operator")));
    }

    // ---- expressions: calls & members ---------------------------------------

    #[test]
    fn call_vs_method_vs_app() {
        let m = parse_ok("module m;\nfunction f() -> int == { g(1, x: 2) }\n");
        let ExprKind::Call(c) = &tail(&m).kind else { panic!() };
        assert_eq!(c.name.node, "g");
        assert_eq!(c.args.len(), 2);
        assert_eq!(c.args[1].name.as_ref().unwrap().node, "x");

        let m = parse_ok("module m;\nfunction f() -> int == { a.b(1) }\n");
        let ExprKind::MethodCall { recv, name, args } = &tail(&m).kind else { panic!() };
        assert!(matches!(recv.kind, ExprKind::Var(_)));
        assert_eq!(name.node, "b");
        assert_eq!(args.len(), 1);

        let m = parse_ok("module m;\nfunction f() -> int == { (lambda(x) { x })(1) }\n");
        assert!(matches!(tail(&m).kind, ExprKind::App { .. }));
    }

    #[test]
    fn turbofish_and_field_and_proj() {
        let m = parse_ok("module m;\nfunction f() -> int == { g::<int, string>(1) }\n");
        let ExprKind::Call(c) = &tail(&m).kind else { panic!() };
        let targs = c.type_args.as_ref().expect("type args");
        assert_eq!(targs.len(), 2);

        let m = parse_ok("module m;\nfunction f() -> int == { r.field }\n");
        let ExprKind::Field { name, .. } = &tail(&m).kind else { panic!() };
        assert_eq!(name.node, "field");

        let m = parse_ok("module m;\nfunction f() -> int == { t.0 }\n");
        let ExprKind::TupleProj { index, .. } = &tail(&m).kind else { panic!() };
        assert_eq!(*index, 0);
    }

    // ---- expressions: collections & control ---------------------------------

    #[test]
    fn collection_forms() {
        let m = parse_ok(
            "module m;\nfunction f() -> int == { let a == set { 1, 2 }; let b == set { x \\in xs if x > 0 }; let c == set { f(x, y) : x \\in xs, y \\in ys }; let d == bag { 1 }; let e == bag { x : x \\in xs }; let g == map { 1: \"a\" }; let h == [1, 2]; let i == (1, 2); 0 }\n",
        );
        let ExprKind::Block { lets, .. } = &body(&m).kind else { panic!() };
        assert!(matches!(lets[0].value.kind, ExprKind::SetLiteral(_)));
        let ExprKind::SetFilter { pat, pred, .. } = &lets[1].value.kind else { panic!() };
        assert!(matches!(pat.kind, PatternKind::Bind(_)));
        assert!(matches!(pred.kind, ExprKind::BinOp { op: BinOpKind::Gt, .. }));
        let ExprKind::SetMap { gens, .. } = &lets[2].value.kind else { panic!() };
        assert_eq!(gens.len(), 2);
        assert!(matches!(lets[3].value.kind, ExprKind::BagLiteral(_)));
        assert!(matches!(lets[4].value.kind, ExprKind::BagMap { .. }));
        assert!(matches!(lets[5].value.kind, ExprKind::MapLit(_)));
        assert!(matches!(lets[6].value.kind, ExprKind::Vector(_)));
        assert!(matches!(lets[7].value.kind, ExprKind::Tuple(_)));
    }

    #[test]
    fn record_and_update() {
        let m = parse_ok("module m;\nfunction f() -> int == { let r == record { a: 1 }; let s == record { r with a: 2 }; 0 }\n");
        let ExprKind::Block { lets, .. } = &body(&m).kind else { panic!() };
        let ExprKind::RecordLit { fields } = &lets[0].value.kind else { panic!() };
        assert_eq!(fields[0].name.node, "a");
        let ExprKind::RecordUpd { base, fields } = &lets[1].value.kind else { panic!() };
        assert!(matches!(base.kind, ExprKind::Var(_)));
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn if_match_quantifier_option() {
        let m = parse_ok("module m;\nfunction f() -> int == { if c then 1 else 2 }\n");
        assert!(matches!(tail(&m).kind, ExprKind::If { .. }));

        let m = parse_ok("module m;\nfunction f() -> int == { match o { some(v) => v, none => 0 } }\n");
        let ExprKind::Match { arms, .. } = &tail(&m).kind else { panic!() };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[0].pat.kind, PatternKind::Some(_)));
        assert!(matches!(arms[1].pat.kind, PatternKind::None));

        let m = parse_ok("module m;\nfunction f() -> bool == { \\A x \\in xs, y \\in ys : x = y }\n");
        let ExprKind::Quantifier { kind, gens, .. } = &tail(&m).kind else { panic!() };
        assert_eq!(*kind, QuantKind::Forall);
        assert_eq!(gens.len(), 2);

        let m = parse_ok("module m;\nfunction f() -> int == { let a == some(1); let b == none; 0 }\n");
        let ExprKind::Block { lets, .. } = &body(&m).kind else { panic!() };
        assert!(matches!(lets[0].value.kind, ExprKind::OptionSome(_)));
        assert!(matches!(lets[1].value.kind, ExprKind::OptionNone));
    }

    #[test]
    fn lambda_full_form() {
        let m = parse_ok(
            "module m;\nfunction f() -> int == { let g == lambda [a, b](x: int, (y, z)) -> int { x + y + z + a + b }; g(1, (2, 3)) }\n",
        );
        let ExprKind::Block { lets, .. } = &body(&m).kind else { panic!() };
        let ExprKind::Lambda(l) = &lets[0].value.kind else { panic!() };
        assert_eq!(l.captures.len(), 2);
        assert_eq!(l.params.len(), 2);
        assert!(l.params[0].ty.is_some());
        assert!(l.params[1].ty.is_none());
        assert!(matches!(l.params[1].pat.kind, PatternKind::Tuple(_)));
        assert!(l.ret.is_some());
        assert!(matches!(l.body.kind, ExprKind::Block { .. }));
    }

    // ---- patterns -----------------------------------------------------------

    #[test]
    fn pattern_forms() {
        let m = parse_ok(
            "module m;\nfunction f() -> int == { match v { (a, b) => a, {x, y} => x, [h, ..t] => h, [] => 0, _ => 0, 1 => 1, \"s\" => 2, true => 3, none => 4, some(w) => w, foo(z) => z } }\n",
        );
        let ExprKind::Match { arms, .. } = &tail(&m).kind else { panic!() };
        let kinds: Vec<&PatternKind> = arms.iter().map(|a| &a.pat.kind).collect();
        assert!(matches!(kinds[0], PatternKind::Tuple(_)));
        assert!(matches!(kinds[1], PatternKind::Record(_)));
        assert!(matches!(kinds[2], PatternKind::Cons { .. }));
        assert!(matches!(kinds[3], PatternKind::ConsNil));
        assert!(matches!(kinds[4], PatternKind::Wildcard));
        assert!(matches!(kinds[5], PatternKind::Lit(PatLit::Int(1))));
        assert!(matches!(kinds[6], PatternKind::Lit(PatLit::Str(_))));
        assert!(matches!(kinds[7], PatternKind::Lit(PatLit::Bool(true))));
        assert!(matches!(kinds[8], PatternKind::None));
        assert!(matches!(kinds[9], PatternKind::Some(_)));
        assert!(matches!(kinds[10], PatternKind::Variant { .. }));
    }

    // ---- literals -----------------------------------------------------------

    #[test]
    fn int_literals() {
        let m = parse_ok("module m;\nfunction f() -> int == { 0x1F + 1_000 }\n");
        let ExprKind::BinOp { lhs, rhs, .. } = &tail(&m).kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::Lit(Literal::Int(31))));
        assert!(matches!(rhs.kind, ExprKind::Lit(Literal::Int(1000))));

        let bag = parse_err("module m;\nfunction f() -> int == { 99999999999999999999999 }\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("overflows")));
        let bag = parse_err("module m;\nfunction f() -> int == { 0xFFFFFFFFFFFFFFFFF }\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("overflows")));
    }

    #[test]
    fn float_literals() {
        let m = parse_ok("module m;\nfunction f() -> float == { 1_000.5 + 1e-3 }\n");
        let ExprKind::BinOp { lhs, rhs, .. } = &tail(&m).kind else { panic!() };
        assert!(matches!(lhs.kind, ExprKind::Lit(Literal::Float(v)) if v == 1000.5));
        assert!(matches!(rhs.kind, ExprKind::Lit(Literal::Float(v)) if v == 0.001));
    }

    #[test]
    fn string_escapes_and_interpolation() {
        let m = parse_ok("module m;\nfunction f() -> string == { \"a\\n\\t\\\\\\\"b\\u{41}\" }\n");
        let ExprKind::Lit(Literal::Str(s)) = &tail(&m).kind else { panic!() };
        assert_eq!(s, "a\n\t\\\"bA");

        let m = parse_ok("module m;\nfunction f() -> string == { \"x \\(1 + 2) y \\(z)!\" }\n");
        let ExprKind::StrInterp(parts) = &tail(&m).kind else { panic!() };
        assert_eq!(parts.len(), 5);
        assert!(matches!(&parts[0], StrPart::Lit(s) if s == "x "));
        assert!(matches!(parts[1], StrPart::Interp(_)));
        assert!(matches!(&parts[2], StrPart::Lit(s) if s == " y "));
        assert!(matches!(parts[3], StrPart::Interp(_)));
        assert!(matches!(&parts[4], StrPart::Lit(s) if s == "!"));
    }

    #[test]
    fn date_and_decimal_literals() {
        let m = parse_ok("module m;\nfunction f() -> date == { date \"2024-02-29\" }\n");
        let ExprKind::Lit(Literal::Date { year: 2024, month: 2, day: 29 }) = &tail(&m).kind else {
            panic!("{:?}", tail(&m).kind)
        };

        let bag = parse_err("module m;\nfunction f() -> date == { date \"2024-1-1\" }\n");
        assert!(msgs(&bag).iter().any(|m| m.contains("malformed date")));

        let m = parse_ok("module m;\nfunction f() -> decimal == { decimal(4,2) 3.14 }\n");
        let ExprKind::Lit(Literal::Decimal { repr, precision }) = &tail(&m).kind else { panic!() };
        assert_eq!(repr, "3.14");
        assert_eq!(*precision, Some((4, 2)));

        let m = parse_ok("module m;\nfunction f() -> decimal == { decimal 1_000 }\n");
        let ExprKind::Lit(Literal::Decimal { repr, precision }) = &tail(&m).kind else { panic!() };
        assert_eq!(repr, "1000");
        assert_eq!(*precision, None);
    }

    // ---- temporal -----------------------------------------------------------

    #[test]
    fn temporal_forms() {
        let m = parse_ok("module m;\nproperty p == [](total_balance()' = total_balance())\n");
        let Item::Property(p) = &m.items[0] else { panic!() };
        let TemporalExpr::Always(inner) = &p.body else { panic!() };
        let TemporalExpr::State(e) = &**inner else { panic!("expected State, got {inner:?}") };
        let ExprKind::BinOp { op: BinOpKind::Eq, lhs, rhs } = &e.kind else { panic!() };
        // Left side is primed, right side is not.
        assert!(matches!(lhs.kind, ExprKind::Primed(_)));
        assert!(matches!(rhs.kind, ExprKind::Call(_)));

        let m = parse_ok("module m;\nproperty p == <> a() ~> b() until c()\n");
        let Item::Property(p) = &m.items[0] else { panic!() };
        // until is the loosest: (<>a ~> b) until c
        let TemporalExpr::Until { lhs, rhs } = &p.body else { panic!("{:?}", p.body) };
        let TemporalExpr::LeadsTo { lhs, .. } = &**lhs else { panic!() };
        assert!(matches!(**lhs, TemporalExpr::Eventually(_)));
        assert!(matches!(**rhs, TemporalExpr::State(_)));

        let m = parse_ok("module m;\nproperty p == \\A a \\in accounts : a.balance >= 0\n");
        let Item::Property(p) = &m.items[0] else { panic!() };
        let TemporalExpr::State(e) = &p.body else { panic!() };
        assert!(matches!(e.kind, ExprKind::Quantifier { .. }));
    }

    // ---- syntax errors ------------------------------------------------------

    #[test]
    fn error_and_missing_nodes_reported() {
        let bag = parse_err("module m\nfunction f() -> int == { 1 + }\n");
        let ms = msgs(&bag);
        assert!(ms.iter().any(|m| m.contains("syntax error")), "{ms:?}");

        let bag = parse_err("this is not cql at all");
        assert!(bag.has_errors());
    }

    #[test]
    fn spans_track_source() {
        let m = parse_ok("module m;\nfunction f() -> int == { 42 }\n");
        let src = "module m;\nfunction f() -> int == { 42 }\n";
        let ExprKind::Lit(Literal::Int(42)) = &tail(&m).kind else { panic!() };
        let sp = tail(&m).span;
        assert_eq!(&src[sp.start as usize..sp.end as usize], "42");
        let Item::Operator(o) = &m.items[0] else { panic!() };
        let nsp = o.name.span;
        assert_eq!(&src[nsp.start as usize..nsp.end as usize], "f");
    }

    // ---- frontend -----------------------------------------------------------

    #[test]
    fn frontend_ok() {
        let src = "module m;\nfunction add(a: int, b: int) -> int == { a + b }\n";
        let (typed, bag) = frontend(src);
        assert!(typed.is_some(), "{}", bag.render());
        assert!(bag.is_empty(), "{}", bag.render());
    }

    #[test]
    fn frontend_collects_type_errors() {
        let src = "module m;\nfunction f() -> int == { true }\n";
        let (typed, bag) = frontend(src);
        assert!(typed.is_none());
        assert!(bag.has_errors());
    }

    #[test]
    fn frontend_isolates_operator_errors() {
        // One broken operator must not prevent checking the other.
        let src = "module m;\nfunction bad() -> int == { true }\nfunction good() -> int == { 1 }\n";
        let (typed, bag) = frontend(src);
        assert!(typed.is_none());
        assert_eq!(bag.error_count(), 1, "{}", bag.render());
    }
}
