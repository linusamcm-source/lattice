//! Generic `tree-sitter` extractor lowering a non-Rust source file to the same
//! structural CLV graph contribution the `syn` path produces (file → function →
//! variable, joined by `contains` edges).
//!
//! A small [`LanguageConfig`] describes, per language, which parse-tree node
//! kinds introduce a `function` node and which introduce a local `variable`
//! binding, plus how to read a binding's name. The shared [`extract`] walk drives
//! any configured language; [`parse_python`] is the Phase-2 Python entry. Tree
//! positions are **0-based rows**, so the walk converts `row + 1` to the CLV
//! [`Range`]'s **1-based** lines (columns stay 0-based), matching the `syn` path.
//!
//! Per the Phase-2 error model (`SPEC.md` §11.1) the extractor is **panic-free**
//! and recovers partial results: every function/variable it can read from the
//! (possibly partial) tree is still emitted, and when the parse tree's root
//! reports a syntax error the `file` node's status is set to [`NodeStatus::Error`]
//! as a coarse "this file has a problem" flag. Recovered siblings stay live.

use tree_sitter::{Language, Node as TsNode, Parser};

use super::{file_node, ParsedFile};
use crate::wire::{edge_id, node_id, Edge, EdgeKind, Meta, Node, NodeStatus, NodeType, Range};

/// Per-language rules driving the generic [`extract`] tree-sitter walk.
///
/// Functions are always named via the grammar's `name` field; only the
/// local-binding rule ([`LanguageConfig::binding_name`]) is language-specific.
struct LanguageConfig {
    /// `meta.language` tag emitted on every function/variable node (e.g. `"python"`).
    language: &'static str,
    /// The tree-sitter grammar loaded into the parser.
    grammar: Language,
    /// Node kinds that introduce a `function` node (name from the `name` field).
    function_kinds: &'static [&'static str],
    /// Node kinds that introduce a local `variable` binding inside a function body.
    binding_kinds: &'static [&'static str],
    /// Reads a binding node's name when it is a single-identifier target, else
    /// `None` (e.g. a tuple unpack), in which case the binding is skipped — such
    /// multi-target bindings are out of Phase-2 scope.
    binding_name: fn(&TsNode, &[u8]) -> Option<String>,
}

/// Parses Python `source` at repo-relative `path` into its structural graph.
///
/// Emits a `file` node, one `function` node per `function_definition` (parented
/// to the file, `meta.language = "python"`), and one `variable` node per simple
/// single-identifier `assignment` inside a function body (parented to its nearest
/// enclosing function), each joined by a `contains` edge. Panic-free: malformed
/// input yields the recovered nodes plus a `file` node with status
/// [`NodeStatus::Error`] when the parse tree reports a syntax error.
pub fn parse_python(path: &str, source: &str) -> ParsedFile {
    extract(path, source, &python_config())
}

/// Returns the [`LanguageConfig`] for Python (`function_definition` functions,
/// single-identifier `assignment` locals).
fn python_config() -> LanguageConfig {
    LanguageConfig {
        language: "python",
        grammar: tree_sitter_python::LANGUAGE.into(),
        function_kinds: &["function_definition"],
        binding_kinds: &["assignment"],
        binding_name: python_binding_name,
    }
}

/// Reads a Python `assignment` target name when its `left` field is a single
/// `identifier`; returns `None` for tuple/attribute/subscript targets.
fn python_binding_name(node: &TsNode, source: &[u8]) -> Option<String> {
    let left = node.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    left.utf8_text(source).ok().map(str::to_string)
}

/// Lowers `source` at `path` to a [`ParsedFile`] using `config`'s rules.
///
/// Parses with a [`Parser`], emits a `file` node (status [`NodeStatus::Error`]
/// when the parse tree's root reports a syntax error, else [`NodeStatus::Unknown`]),
/// then [`walk`]s the named tree to emit `function`/`variable` nodes and their
/// `contains` edges. Panic-free: a grammar-load failure or a `None` parse (which
/// the static grammars do not produce) recovers to a single `error`-status file
/// node.
fn extract(path: &str, source: &str, config: &LanguageConfig) -> ParsedFile {
    let file_id = node_id(NodeType::File, path, "");
    let label = file_label(path);

    let mut parser = Parser::new();
    if parser.set_language(&config.grammar).is_err() {
        return file_only(file_id, label, NodeStatus::Error);
    }
    let Some(tree) = parser.parse(source, None) else {
        return file_only(file_id, label, NodeStatus::Error);
    };

    let root = tree.root_node();
    let status = if root.has_error() {
        NodeStatus::Error
    } else {
        NodeStatus::Unknown
    };

    let mut nodes = vec![file_node(file_id.clone(), label, status)];
    let mut edges = Vec::new();
    walk(
        root,
        source.as_bytes(),
        config,
        &file_id,
        path,
        None,
        &mut nodes,
        &mut edges,
    );

    ParsedFile { nodes, edges }
}

/// Recursively visits `node`'s named children, emitting `function`/`variable`
/// nodes and their `contains` edges per `config`.
///
/// `current_fn` is the `(id, name)` of the nearest enclosing function, or `None`
/// at module scope. A `function_kinds` child emits a `function` node (parented to
/// the file) and recurses with itself as the new `current_fn`, so a binding is
/// attributed to its **nearest** enclosing function (an inner function's locals
/// never leak to an outer one). A `binding_kinds` child emits a `variable` node
/// (parented to `current_fn`) only when it is inside a function and yields a
/// single-identifier name via [`LanguageConfig::binding_name`].
#[allow(clippy::too_many_arguments)]
fn walk(
    node: TsNode,
    source: &[u8],
    config: &LanguageConfig,
    file_id: &str,
    path: &str,
    current_fn: Option<(&str, &str)>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if config.function_kinds.contains(&kind) {
            if let Some(name) = field_text(&child, "name", source) {
                let fn_id = node_id(NodeType::Function, path, &name);
                push_node(
                    nodes,
                    &fn_id,
                    NodeType::Function,
                    &name,
                    file_id,
                    config,
                    path,
                    &child,
                );
                push_edge(edges, file_id, &fn_id);
                walk(
                    child,
                    source,
                    config,
                    file_id,
                    path,
                    Some((fn_id.as_str(), name.as_str())),
                    nodes,
                    edges,
                );
                continue;
            }
        } else if config.binding_kinds.contains(&kind) {
            if let (Some((fn_id, fn_name)), Some(var_name)) =
                (current_fn, (config.binding_name)(&child, source))
            {
                let symbol = format!("{fn_name}:{var_name}");
                let var_id = node_id(NodeType::Variable, path, &symbol);
                push_node(
                    nodes,
                    &var_id,
                    NodeType::Variable,
                    &var_name,
                    fn_id,
                    config,
                    path,
                    &child,
                );
                push_edge(edges, fn_id, &var_id);
            }
        }
        walk(
            child, source, config, file_id, path, current_fn, nodes, edges,
        );
    }
}

/// Appends a structural node with `meta.language`/`file_path`/`range` filled.
#[allow(clippy::too_many_arguments)]
fn push_node(
    nodes: &mut Vec<Node>,
    id: &str,
    node_type: NodeType,
    label: &str,
    parent_id: &str,
    config: &LanguageConfig,
    path: &str,
    src_node: &TsNode,
) {
    nodes.push(Node {
        id: id.to_string(),
        node_type,
        label: label.to_string(),
        parent_id: Some(parent_id.to_string()),
        child_ids: Vec::new(),
        status: NodeStatus::Unknown,
        docs: None,
        signature: None,
        meta: Some(Meta {
            language: Some(config.language.to_string()),
            file_path: Some(path.to_string()),
            range: Some(ts_range(src_node)),
        }),
    });
}

/// Appends a `contains` edge from `source` to `target`.
fn push_edge(edges: &mut Vec<Edge>, source: &str, target: &str) {
    edges.push(Edge {
        id: edge_id(source, target),
        source: source.to_string(),
        target: target.to_string(),
        kind: EdgeKind::Contains,
        hot: false,
    });
}

/// Reads the UTF-8 text of `node`'s named `field` child, if present and valid.
fn field_text(node: &TsNode, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)?
        .utf8_text(source)
        .ok()
        .map(str::to_string)
}

/// Converts a tree-sitter node span to a CLV [`Range`] (1-based line via
/// `row + 1`, 0-based column).
fn ts_range(node: &TsNode) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32 + 1,
        start_col: start.column as u32,
        end_line: end.row as u32 + 1,
        end_col: end.column as u32,
    }
}

/// Builds a [`ParsedFile`] holding only the file node with the given `status`.
fn file_only(file_id: String, label: String, status: NodeStatus) -> ParsedFile {
    ParsedFile {
        nodes: vec![file_node(file_id, label, status)],
        edges: Vec::new(),
    }
}

/// Returns a file node's display label: the path's basename, or the whole path.
fn file_label(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn ids(parsed: &ParsedFile) -> Vec<&str> {
        parsed.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    fn has_contains_edge(parsed: &ParsedFile, source: &str, target: &str) -> bool {
        parsed
            .edges
            .iter()
            .any(|e| e.source == source && e.target == target && e.kind == EdgeKind::Contains)
    }

    #[test]
    fn two_defs_yield_file_and_exactly_two_function_nodes() {
        let parsed = parse_python("a.py", "def foo():\n    pass\n\ndef bar():\n    pass\n");
        assert!(ids(&parsed).contains(&"file:a.py"), "missing file node");
        let fns = function_nodes(&parsed);
        assert_eq!(fns.len(), 2, "expected exactly two function nodes: {fns:?}");
        for id in ["fn:a.py:foo", "fn:a.py:bar"] {
            let node = fns
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("missing function node {id}"));
            assert_eq!(node.parent_id.as_deref(), Some("file:a.py"));
            assert!(
                has_contains_edge(&parsed, "file:a.py", id),
                "missing contains edge file:a.py -> {id}"
            );
        }
    }

    #[test]
    fn assignments_yield_variable_nodes_with_parents_and_edges() {
        let parsed = parse_python("a.py", "def f():\n    x = 1\n    y = 2\n");
        let vars = variable_nodes(&parsed);
        assert_eq!(
            vars.len(),
            2,
            "expected exactly two variable nodes: {vars:?}"
        );
        for (id, label) in [("var:a.py:f:x", "x"), ("var:a.py:f:y", "y")] {
            let node = vars
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("missing variable node {id}"));
            assert_eq!(node.label, label);
            assert_eq!(node.parent_id.as_deref(), Some("fn:a.py:f"));
            assert!(
                has_contains_edge(&parsed, "fn:a.py:f", id),
                "missing contains edge fn:a.py:f -> {id}"
            );
        }
    }

    #[test]
    fn python_nodes_carry_language_and_one_based_range() {
        let parsed = parse_python("a.py", "def f():\n    x = 1\n");
        let scoped: Vec<&Node> = function_nodes(&parsed)
            .into_iter()
            .chain(variable_nodes(&parsed))
            .collect();
        assert!(!scoped.is_empty(), "expected function/variable nodes");
        for node in scoped {
            let meta = node.meta.as_ref().expect("node has meta");
            assert_eq!(meta.language.as_deref(), Some("python"));
            let range = meta.range.expect("node has a range");
            assert!(range.start_line > 0, "startLine must be 1-based: {range:?}");
        }
    }

    #[test]
    fn malformed_python_does_not_panic_and_marks_file_error() {
        let result = std::panic::catch_unwind(|| parse_python("a.py", "def (:\n"));
        let parsed = result.expect("parse_python must not panic on malformed input");
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:a.py")
            .expect("file node present");
        assert_eq!(file.node_type, NodeType::File);
        assert_eq!(file.status, NodeStatus::Error);
    }

    #[test]
    fn module_level_assignment_is_not_attributed_to_any_function() {
        let parsed = parse_python("a.py", "z = 1\ndef f():\n    x = 1\n");
        let vars: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(
            vars.contains(&"var:a.py:f:x"),
            "missing fn-local x: {vars:?}"
        );
        assert!(
            !vars.iter().any(|id| id.ends_with(":z")),
            "module-level z must not be attributed to a function: {vars:?}"
        );
    }

    #[test]
    fn tuple_target_assignment_is_skipped_but_single_target_kept() {
        let parsed = parse_python("a.py", "def f():\n    a, b = 1, 2\n    c = 3\n");
        let vars: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(
            vars,
            vec!["var:a.py:f:c"],
            "only single-target c expected: {vars:?}"
        );
    }

    #[test]
    fn nested_function_assignment_attributes_to_inner_function() {
        let parsed = parse_python(
            "a.py",
            "def outer():\n    a = 1\n    def inner():\n        b = 2\n",
        );
        let vars: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(
            vars.contains(&"var:a.py:outer:a"),
            "outer.a missing: {vars:?}"
        );
        assert!(
            vars.contains(&"var:a.py:inner:b"),
            "inner.b missing: {vars:?}"
        );
        assert!(
            !vars.contains(&"var:a.py:outer:b"),
            "inner b must not attribute to outer: {vars:?}"
        );
        assert!(has_contains_edge(
            &parsed,
            "fn:a.py:inner",
            "var:a.py:inner:b"
        ));
    }
}
