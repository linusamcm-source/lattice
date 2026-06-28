//! Lowers a single source file to its structural CLV graph contribution.
//!
//! Rust files go through [`parse_rust_source`] (`syn`); Python files go through
//! [`parse_python`], a `tree-sitter`-backed path (see the private [`treesitter`]
//! submodule) that emits the **same** file → function → variable node/edge model
//! with `meta.language = "python"`. Both produce a [`ParsedFile`] and recover
//! panic-free from syntax errors.
//!
//! [`parse_rust_source`] uses `syn` to parse one file into a [`ParsedFile`]: a
//! `file` [`Node`] plus one `function` node per free `fn` and per method declared
//! in an `impl`/`trait` block, and — for each function with a body — one
//! `variable` node per simple `let`-bound identifier in that body. Nodes are
//! joined by `contains` [`Edge`]s (file → function, function → variable). Ids come
//! from the §A.1 helpers ([`node_id`]/[`edge_id`]) so elements keep their identity
//! across runs, and every node carries a [`Meta::range`] sourced from the item's
//! span (`proc-macro2`'s `span-locations` feature is required, else spans report
//! line/col `0`). Variable bindings are deduplicated by name with the latest
//! shadowing binding winning, so the contribution holds no duplicate ids or edges.
//!
//! Per `SPEC.md` §6/§11.1 the parser is **panic-free**: malformed input never
//! aborts. When `syn::parse_file` rejects the source, the function returns a
//! [`ParsedFile`] holding only the file node with [`NodeStatus::Error`] and no
//! function nodes, so a downstream graph still receives a partial, well-formed
//! contribution. The caller supplies an already repo-relative `path`
//! (normalisation is a separate concern).

use proc_macro2::Span;

use crate::wire::{edge_id, node_id, Edge, EdgeKind, Meta, Node, NodeStatus, NodeType, Range};

mod treesitter;

pub use treesitter::parse_python;

/// One Rust file's structural contribution to the CLV graph.
///
/// Holds the [`Node`]s (one `file` node, its `function` children, and each
/// function's `variable` children) and the `contains` [`Edge`]s linking them
/// (file → function, function → variable), as produced by [`parse_rust_source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// The file node, its function children, and each function's variable children.
    pub nodes: Vec<Node>,
    /// `contains` edges: file → function and function → variable.
    pub edges: Vec<Edge>,
}

/// Parses Rust `source` at repo-relative `path` into its structural graph.
///
/// Returns a [`ParsedFile`] with a `file` node (label = the path's basename,
/// status [`NodeStatus::Unknown`]) and one `function` node per free `fn` and per
/// `impl`/`trait` method, each parented to the file with a `contains` edge and a
/// [`Meta::range`] filled from the item's span. For every function with a body,
/// one `variable` node is emitted per simple `let`-bound identifier (id
/// `node_id(Variable, path, "<fn>:<name>")`), parented to its function with a
/// `contains` edge; shadowed bindings dedupe to the latest by name.
///
/// Recovery (panic-free): if `syn::parse_file` fails, the returned [`ParsedFile`]
/// contains only the file node with status [`NodeStatus::Error`] and no functions
/// or variables.
pub fn parse_rust_source(path: &str, source: &str) -> ParsedFile {
    let file_id = node_id(NodeType::File, path, "");
    let label = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string();

    // On a syntax error, recover with just an `error`-status file node.
    let ast = match syn::parse_file(source) {
        Ok(ast) => ast,
        Err(_) => {
            return ParsedFile {
                nodes: vec![file_node(file_id, label, NodeStatus::Error)],
                edges: Vec::new(),
            };
        }
    };

    let mut nodes = vec![file_node(file_id.clone(), label, NodeStatus::Unknown)];
    let mut edges = Vec::new();

    for item in &ast.items {
        match item {
            syn::Item::Fn(item_fn) => {
                push_function(
                    &mut nodes,
                    &mut edges,
                    &file_id,
                    path,
                    &item_fn.sig.ident,
                    Some(item_fn.block.as_ref()),
                );
            }
            syn::Item::Impl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let syn::ImplItem::Fn(method) = impl_item {
                        push_function(
                            &mut nodes,
                            &mut edges,
                            &file_id,
                            path,
                            &method.sig.ident,
                            Some(&method.block),
                        );
                    }
                }
            }
            syn::Item::Trait(item_trait) => {
                for trait_item in &item_trait.items {
                    if let syn::TraitItem::Fn(method) = trait_item {
                        push_function(
                            &mut nodes,
                            &mut edges,
                            &file_id,
                            path,
                            &method.sig.ident,
                            method.default.as_ref(),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    ParsedFile { nodes, edges }
}

/// Builds a bare `file` node with the given id, label, and status.
fn file_node(id: String, label: String, status: NodeStatus) -> Node {
    Node {
        id,
        node_type: NodeType::File,
        label,
        parent_id: None,
        child_ids: Vec::new(),
        status,
        docs: None,
        signature: None,
        meta: None,
    }
}

/// Appends a `function` node for `ident` plus its `contains` edge from the file,
/// then extracts that function's `let`-bound variables when `body` is present.
///
/// The function node id is `node_id(Function, path, name)`, the label is the
/// function name, the parent is `file_id`, and the range is taken from the
/// identifier's span (1-based line, 0-based column) via [`span_range`]. When
/// `body` is `Some` (free fns and `impl`/`trait` methods that have a block) its
/// simple `let` bindings are lowered to `variable` children via
/// [`push_variables`]; a trait method declared without a body (`body == None`)
/// contributes no variables.
fn push_function(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    file_id: &str,
    path: &str,
    ident: &syn::Ident,
    body: Option<&syn::Block>,
) {
    let name = ident.to_string();
    let fn_id = node_id(NodeType::Function, path, &name);
    nodes.push(Node {
        id: fn_id.clone(),
        node_type: NodeType::Function,
        label: name.clone(),
        parent_id: Some(file_id.to_string()),
        child_ids: Vec::new(),
        status: NodeStatus::Unknown,
        docs: None,
        signature: None,
        meta: Some(Meta {
            language: Some("rust".to_string()),
            file_path: Some(path.to_string()),
            range: Some(span_range(ident.span())),
        }),
    });
    edges.push(Edge {
        id: edge_id(file_id, &fn_id),
        source: file_id.to_string(),
        target: fn_id.clone(),
        kind: EdgeKind::Contains,
        hot: false,
    });
    if let Some(block) = body {
        push_variables(nodes, edges, &fn_id, &name, path, block);
    }
}

/// Appends a deduplicated `variable` node and `contains` edge for each simple
/// `let`-bound identifier in `block`, parented to the owning function.
///
/// Walks `block.stmts` for `let` statements and collects every simple bound
/// identifier from each binding pattern (including idents nested in tuple and
/// struct patterns) via [`collect_pattern_idents`]. Bindings are deduplicated by
/// name with the **latest** binding's span winning, so shadowing
/// (`let x = 1; let x = 2;`) yields a single `var:<path>:<fn>:x` node and one
/// edge — matching the graph's upsert-by-id semantics. Each node id is
/// `node_id(Variable, path, "<fn>:<name>")`, the label is the binding name, the
/// parent is `fn_id`, and the range is taken from the binding's span.
fn push_variables(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    fn_id: &str,
    fn_name: &str,
    path: &str,
    block: &syn::Block,
) {
    let mut bindings: Vec<(String, Span)> = Vec::new();
    for stmt in &block.stmts {
        if let syn::Stmt::Local(local) = stmt {
            let mut idents: Vec<(String, Span)> = Vec::new();
            collect_pattern_idents(&local.pat, &mut idents);
            for (name, span) in idents {
                match bindings.iter_mut().find(|(existing, _)| *existing == name) {
                    Some(entry) => entry.1 = span,
                    None => bindings.push((name, span)),
                }
            }
        }
    }
    for (var_name, span) in &bindings {
        let symbol = format!("{fn_name}:{var_name}");
        let var_id = node_id(NodeType::Variable, path, &symbol);
        nodes.push(Node {
            id: var_id.clone(),
            node_type: NodeType::Variable,
            label: var_name.clone(),
            parent_id: Some(fn_id.to_string()),
            child_ids: Vec::new(),
            status: NodeStatus::Unknown,
            docs: None,
            signature: None,
            meta: Some(Meta {
                language: Some("rust".to_string()),
                file_path: Some(path.to_string()),
                range: Some(span_range(*span)),
            }),
        });
        edges.push(Edge {
            id: edge_id(fn_id, &var_id),
            source: fn_id.to_string(),
            target: var_id,
            kind: EdgeKind::Contains,
            hot: false,
        });
    }
}

/// Collects every simple bound identifier in `pat`, paired with its span.
///
/// Recurses through tuple, tuple-struct, struct, and type-ascription patterns so
/// `let (a, b) = ..` contributes both `a` and `b` and `let c: i32 = ..`
/// contributes `c`. Non-identifier pattern pieces (`_` wildcards, literals, rest
/// `..`) contribute nothing.
fn collect_pattern_idents(pat: &syn::Pat, out: &mut Vec<(String, Span)>) {
    match pat {
        syn::Pat::Ident(pat_ident) => {
            out.push((pat_ident.ident.to_string(), pat_ident.ident.span()));
        }
        syn::Pat::Tuple(tuple) => {
            for elem in &tuple.elems {
                collect_pattern_idents(elem, out);
            }
        }
        syn::Pat::TupleStruct(tuple_struct) => {
            for elem in &tuple_struct.elems {
                collect_pattern_idents(elem, out);
            }
        }
        syn::Pat::Struct(pat_struct) => {
            for field in &pat_struct.fields {
                collect_pattern_idents(&field.pat, out);
            }
        }
        syn::Pat::Type(pat_type) => {
            collect_pattern_idents(&pat_type.pat, out);
        }
        _ => {}
    }
}

/// Converts a `proc-macro2` [`Span`] to a CLV [`Range`] (1-based line, 0-based col).
///
/// Requires `proc-macro2`'s `span-locations` feature; without it spans report
/// line/column `0` and the [`Range`] would be meaningless.
fn span_range(span: Span) -> Range {
    let start = span.start();
    let end = span.end();
    Range {
        start_line: start.line as u32,
        start_col: start.column as u32,
        end_line: end.line as u32,
        end_col: end.column as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(parsed: &ParsedFile) -> Vec<&str> {
        parsed.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    fn function_nodes(parsed: &ParsedFile) -> Vec<&Node> {
        parsed
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Function)
            .collect()
    }

    fn variable_nodes(parsed: &ParsedFile) -> Vec<&Node> {
        parsed
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Variable)
            .collect()
    }

    fn has_contains_edge(parsed: &ParsedFile, source: &str, target: &str) -> bool {
        parsed
            .edges
            .iter()
            .any(|e| e.source == source && e.target == target && e.kind == EdgeKind::Contains)
    }

    #[test]
    fn two_let_bindings_yield_two_variable_nodes_with_ids_parents_and_edges() {
        let parsed = parse_rust_source("src/x.rs", "fn f() { let x = 1; let y = 2; }");
        let vars = variable_nodes(&parsed);
        assert_eq!(
            vars.len(),
            2,
            "expected exactly two variable nodes: {vars:?}"
        );
        for (id, label) in [("var:src/x.rs:f:x", "x"), ("var:src/x.rs:f:y", "y")] {
            let node = vars
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("missing variable node {id}"));
            assert_eq!(node.label, label);
            assert_eq!(node.parent_id.as_deref(), Some("fn:src/x.rs:f"));
            assert!(
                has_contains_edge(&parsed, "fn:src/x.rs:f", id),
                "missing contains edge fn:src/x.rs:f -> {id}"
            );
        }
    }

    #[test]
    fn shadowed_let_binding_yields_single_variable_node_and_edge() {
        let parsed = parse_rust_source("src/x.rs", "fn f() { let x = 1; let x = 2; }");
        let vars = variable_nodes(&parsed);
        assert_eq!(vars.len(), 1, "shadowing must dedupe to one node: {vars:?}");
        assert_eq!(vars[0].id, "var:src/x.rs:f:x");
        let edges: Vec<_> = parsed
            .edges
            .iter()
            .filter(|e| e.target == "var:src/x.rs:f:x")
            .collect();
        assert_eq!(edges.len(), 1, "shadowing must yield one contains edge");
    }

    #[test]
    fn impl_method_local_let_yields_variable_node() {
        let parsed = parse_rust_source(
            "src/x.rs",
            "struct S; impl S { fn m(&self) { let z = 1; } }",
        );
        let node = parsed
            .nodes
            .iter()
            .find(|n| n.id == "var:src/x.rs:m:z")
            .unwrap_or_else(|| panic!("missing impl-method variable node"));
        assert_eq!(node.node_type, NodeType::Variable);
        assert_eq!(node.parent_id.as_deref(), Some("fn:src/x.rs:m"));
        assert!(has_contains_edge(
            &parsed,
            "fn:src/x.rs:m",
            "var:src/x.rs:m:z"
        ));
    }

    #[test]
    fn function_without_let_bindings_yields_no_variable_nodes() {
        let parsed = parse_rust_source("src/x.rs", "fn g() {}");
        assert!(ids(&parsed).contains(&"fn:src/x.rs:g"));
        assert!(
            variable_nodes(&parsed).is_empty(),
            "empty-body fn must yield no variables"
        );
    }

    #[test]
    fn variable_range_start_line_is_one_based() {
        let parsed = parse_rust_source("src/x.rs", "fn f() {\n    let x = 1;\n}");
        for node in variable_nodes(&parsed) {
            let meta = node.meta.as_ref().expect("variable node has meta");
            let range = meta.range.expect("variable node has a range");
            assert!(range.start_line > 0, "startLine must be 1-based: {range:?}");
        }
    }

    #[test]
    fn trait_method_without_body_yields_no_variable_nodes() {
        let parsed = parse_rust_source("src/x.rs", "trait T {\n    fn t(&self);\n}");
        assert!(ids(&parsed).contains(&"fn:src/x.rs:t"));
        assert!(
            variable_nodes(&parsed).is_empty(),
            "trait method without a body must yield no variables"
        );
    }

    #[test]
    fn tuple_pattern_let_yields_a_node_per_bound_ident() {
        let parsed = parse_rust_source("src/x.rs", "fn f() { let (a, b) = (1, 2); }");
        let got: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(got.contains(&"var:src/x.rs:f:a"), "missing a: {got:?}");
        assert!(got.contains(&"var:src/x.rs:f:b"), "missing b: {got:?}");
    }

    #[test]
    fn struct_and_tuple_struct_patterns_extract_inner_idents() {
        let parsed = parse_rust_source(
            "src/x.rs",
            "fn f() { let Point { x, y } = p; let Some(z) = q; }",
        );
        let got: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        for want in ["var:src/x.rs:f:x", "var:src/x.rs:f:y", "var:src/x.rs:f:z"] {
            assert!(got.contains(&want), "missing {want}: {got:?}");
        }
    }

    #[test]
    fn typed_let_binding_extracts_the_identifier() {
        let parsed = parse_rust_source("src/x.rs", "fn f() { let c: i32 = 3; }");
        let got: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(got, vec!["var:src/x.rs:f:c"]);
    }

    #[test]
    fn wildcard_and_literal_pattern_pieces_yield_no_node() {
        let parsed = parse_rust_source("src/x.rs", "fn f() { let _ = 5; }");
        assert!(
            variable_nodes(&parsed).is_empty(),
            "wildcard binding must produce no variable node"
        );
    }

    #[test]
    fn extracts_expected_ids_for_each_source_shape() {
        let cases: Vec<(&str, &str, Vec<&str>)> = vec![
            (
                "src/x.rs",
                "fn foo() {}\nfn bar() {}",
                vec!["file:src/x.rs", "fn:src/x.rs:foo", "fn:src/x.rs:bar"],
            ),
            (
                "src/x.rs",
                "struct S;\nimpl S {\n    fn m(&self) {}\n}",
                vec!["file:src/x.rs", "fn:src/x.rs:m"],
            ),
            (
                "src/x.rs",
                "trait T {\n    fn t(&self);\n}",
                vec!["file:src/x.rs", "fn:src/x.rs:t"],
            ),
        ];
        for (path, source, want_ids) in cases {
            let parsed = parse_rust_source(path, source);
            let got = ids(&parsed);
            for want in &want_ids {
                assert!(
                    got.contains(want),
                    "{source:?}: missing id {want}, got {got:?}"
                );
            }
        }
    }

    #[test]
    fn two_free_fns_yield_file_and_exactly_two_function_nodes() {
        let parsed = parse_rust_source("src/x.rs", "fn foo() {}\nfn bar() {}");
        let got = ids(&parsed);
        assert!(got.contains(&"file:src/x.rs"));
        let fns = function_nodes(&parsed);
        assert_eq!(fns.len(), 2, "expected exactly two function nodes: {got:?}");
        let fn_ids: Vec<&str> = fns.iter().map(|n| n.id.as_str()).collect();
        assert!(fn_ids.contains(&"fn:src/x.rs:foo"));
        assert!(fn_ids.contains(&"fn:src/x.rs:bar"));
    }

    #[test]
    fn function_nodes_carry_label_parent_and_contains_edges() {
        let parsed = parse_rust_source("src/x.rs", "fn foo() {}\nfn bar() {}");
        let file_id = "file:src/x.rs";
        for name in ["foo", "bar"] {
            let fn_id = format!("fn:src/x.rs:{name}");
            let node = parsed
                .nodes
                .iter()
                .find(|n| n.id == fn_id)
                .unwrap_or_else(|| panic!("missing function node {fn_id}"));
            assert_eq!(node.label, name);
            assert_eq!(node.parent_id.as_deref(), Some(file_id));
            let has_edge = parsed
                .edges
                .iter()
                .any(|e| e.source == file_id && e.target == fn_id && e.kind == EdgeKind::Contains);
            assert!(has_edge, "missing contains edge {file_id} -> {fn_id}");
        }
    }

    #[test]
    fn method_inside_impl_yields_function_node() {
        let parsed = parse_rust_source("src/x.rs", "struct S;\nimpl S {\n    fn m(&self) {}\n}");
        assert!(ids(&parsed).contains(&"fn:src/x.rs:m"));
    }

    #[test]
    fn malformed_source_does_not_panic_and_yields_error_file_node() {
        let result = std::panic::catch_unwind(|| parse_rust_source("src/x.rs", "fn foo( {"));
        let parsed = result.expect("parse_rust_source must not panic on malformed input");
        assert_eq!(parsed.nodes.len(), 1, "only the file node on parse error");
        assert_eq!(parsed.nodes[0].node_type, NodeType::File);
        assert_eq!(parsed.nodes[0].status, NodeStatus::Error);
        assert!(parsed.edges.is_empty());
    }

    #[test]
    fn function_range_is_populated_with_one_based_lines() {
        let parsed = parse_rust_source("src/x.rs", "fn foo() {}\nfn bar() {}");
        for node in function_nodes(&parsed) {
            let meta = node.meta.as_ref().expect("function node has meta");
            let range = meta.range.expect("function node has a range");
            assert!(range.start_line > 0, "startLine must be 1-based: {range:?}");
            assert!(
                range.end_line >= range.start_line,
                "endLine must be >= startLine: {range:?}"
            );
        }
    }
}
