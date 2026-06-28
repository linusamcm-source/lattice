//! Lowers a single Rust source file to its structural CLV graph contribution.
//!
//! [`parse_rust_source`] uses `syn` to parse one file into a [`ParsedFile`]: a
//! `file` [`Node`] plus one `function` node per free `fn` and per method declared
//! in an `impl`/`trait` block, joined by `contains` [`Edge`]s from the file to each
//! function. Ids come from the §A.1 helpers ([`node_id`]/[`edge_id`]) so elements
//! keep their identity across runs, and every node carries a [`Meta::range`] sourced
//! from the item's span (`proc-macro2`'s `span-locations` feature is required, else
//! spans report line/col `0`).
//!
//! Per `SPEC.md` §6/§11.1 the parser is **panic-free**: malformed input never
//! aborts. When `syn::parse_file` rejects the source, the function returns a
//! [`ParsedFile`] holding only the file node with [`NodeStatus::Error`] and no
//! function nodes, so a downstream graph still receives a partial, well-formed
//! contribution. The caller supplies an already repo-relative `path`
//! (normalisation is a separate concern).

use proc_macro2::Span;

use crate::wire::{edge_id, node_id, Edge, EdgeKind, Meta, Node, NodeStatus, NodeType, Range};

/// One Rust file's structural contribution to the CLV graph.
///
/// Holds the [`Node`]s (one `file` node and its `function` children) and the
/// `contains` [`Edge`]s linking them, as produced by [`parse_rust_source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// The file node followed by its function child nodes.
    pub nodes: Vec<Node>,
    /// `contains` edges from the file node to each function node.
    pub edges: Vec<Edge>,
}

/// Parses Rust `source` at repo-relative `path` into its structural graph.
///
/// Returns a [`ParsedFile`] with a `file` node (label = the path's basename,
/// status [`NodeStatus::Unknown`]) and one `function` node per free `fn` and per
/// `impl`/`trait` method, each parented to the file with a `contains` edge and a
/// [`Meta::range`] filled from the item's span.
///
/// Recovery (panic-free): if `syn::parse_file` fails, the returned [`ParsedFile`]
/// contains only the file node with status [`NodeStatus::Error`] and no functions.
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
                push_function(&mut nodes, &mut edges, &file_id, path, &item_fn.sig.ident);
            }
            syn::Item::Impl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let syn::ImplItem::Fn(method) = impl_item {
                        push_function(&mut nodes, &mut edges, &file_id, path, &method.sig.ident);
                    }
                }
            }
            syn::Item::Trait(item_trait) => {
                for trait_item in &item_trait.items {
                    if let syn::TraitItem::Fn(method) = trait_item {
                        push_function(&mut nodes, &mut edges, &file_id, path, &method.sig.ident);
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

/// Appends a `function` node for `ident` plus its `contains` edge from the file.
///
/// The node id is `node_id(Function, path, name)`, the label is the function
/// name, the parent is `file_id`, and the range is taken from the identifier's
/// span (1-based line, 0-based column) via [`span_range`].
fn push_function(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    file_id: &str,
    path: &str,
    ident: &syn::Ident,
) {
    let name = ident.to_string();
    let fn_id = node_id(NodeType::Function, path, &name);
    nodes.push(Node {
        id: fn_id.clone(),
        node_type: NodeType::Function,
        label: name,
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
        target: fn_id,
        kind: EdgeKind::Contains,
        hot: false,
    });
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
