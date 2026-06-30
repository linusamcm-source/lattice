//! Generic `tree-sitter` extractor lowering a non-Rust source file to the same
//! structural CLV graph contribution the `syn` path produces (file → function →
//! variable, joined by `contains` edges).
//!
//! A small [`LanguageConfig`] describes, per language, which parse-tree node
//! kinds introduce a `function` node and which introduce a local `variable`
//! binding, how to read a binding's name, and how to read a **function**'s
//! documentation. The shared [`extract`] walk drives any configured language;
//! [`parse_python`] is the Phase-2 Python entry (`function_definition` functions,
//! `assignment` locals) and [`parse_typescript`] the TypeScript entry
//! (`function_declaration` functions, `variable_declarator` locals). Tree
//! positions are **0-based rows**, so the walk converts `row + 1` to the CLV
//! [`Range`]'s **1-based** lines (columns stay 0-based), matching the `syn` path.
//!
//! Phase 3 populates each function node's `docs` from its source: a Python
//! docstring ([`python_doc`]) or a TypeScript JSDoc block ([`typescript_doc`]),
//! `None` when undocumented. Variable, file, and class docs remain `None`
//! (deferred per the Phase-3 doc-scope note); ids and structure are unchanged.
//!
//! Phase 4 populates each function node's `signature` ([`crate::wire::Signature`])
//! with its typed parameters and return type: [`python_signature`] reads a
//! `function_definition`'s `parameters`/`return_type` (dropping a leading
//! `self`/`cls`) and [`typescript_signature`] a `function_declaration`'s
//! `formal_parameters`/`return_type`. Absent annotations render as empty-string
//! types (Python/TypeScript are optionally typed). Variable nodes keep
//! `signature: None`; ids, structure, and docs are unchanged.
//!
//! Per the Phase-2 error model (`SPEC.md` §11.1) the extractor is **panic-free**
//! and recovers partial results: every function/variable it can read from the
//! (possibly partial) tree is still emitted, and when the parse tree's root
//! reports a syntax error the `file` node's status is set to [`NodeStatus::Error`]
//! as a coarse, **file-level** "this file has a problem" flag (the individual
//! offending tree-sitter `ERROR` node is *not* marked — a future refinement).
//! Recovered siblings stay live: the [`walk`] has no early return on the error
//! region, so a `function` declared *before* a broken one is still emitted. This
//! is the deliberate counterpart to the Rust `syn` path, which is all-or-nothing
//! (one syntax error discards every sibling, leaving only the file node — see
//! [`super::parse_rust_source`]).

use tree_sitter::{Language, Node as TsNode, Parser};

use super::{file_node, ParsedFile};
use crate::wire::{
    edge_id, node_id, Edge, EdgeKind, Meta, Node, NodeStatus, NodeType, Param, Range, Signature,
};

/// Per-language rules driving the generic [`extract`] tree-sitter walk.
///
/// Functions are always named via the grammar's `name` field; the language-specific
/// rules are the local-binding reader ([`LanguageConfig::binding_name`]), the
/// function documentation reader ([`LanguageConfig::doc_for`]), and the function
/// signature reader ([`LanguageConfig::signature_of`]).
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
    /// Reads a **function** node's documentation (Python docstring / TypeScript
    /// JSDoc) from its parse-tree node, or `None` when the function is
    /// undocumented. Only function nodes consult this rule; variable, file, and
    /// class nodes keep `docs: None` (deferred per the Phase-3 doc-scope note).
    doc_for: fn(&TsNode, &[u8]) -> Option<String>,
    /// Reads a **function** node's [`Signature`] (typed params plus return type)
    /// from its parse-tree node, always `Some` for a function the walk emits.
    /// Absent type annotations render as empty-string types (Python/TypeScript are
    /// optionally typed). Only function nodes consult this rule; variable, file,
    /// and class nodes keep `signature: None`.
    signature_of: fn(&TsNode, &[u8]) -> Option<Signature>,
}

/// Parses Python `source` at repo-relative `path` into its structural graph.
///
/// Emits a `file` node, one `function` node per `function_definition` (parented
/// to the file, `meta.language = "python"`, `docs` set to the function's docstring
/// via [`python_doc`] when present, and `signature` set to its typed params and
/// return type via [`python_signature`]), and one `variable` node per simple
/// single-identifier `assignment` inside a function body (parented to its nearest
/// enclosing function), each joined by a `contains` edge. Panic-free: malformed
/// input yields the recovered nodes plus a `file` node with status
/// [`NodeStatus::Error`] when the parse tree reports a syntax error.
///
/// Crate-private: callers reach the Python path through the extension-dispatching
/// [`super::parse_source`].
pub(crate) fn parse_python(path: &str, source: &str) -> ParsedFile {
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
        doc_for: python_doc,
        signature_of: python_signature,
    }
}

/// Reads a Python `function_definition`'s [`Signature`].
///
/// Walks the `parameters` child: each `identifier` yields an untyped [`Param`]
/// (`param_type == ""`), each `typed_parameter` a [`Param`] whose `name` is its
/// identifier child and whose `param_type` is the `type` field's annotation text
/// (the text after `:`). A leading `self`/`cls` parameter is dropped — it is the
/// receiver, not a data parameter. `returns` is the `return_type` field's text
/// (the annotation after `->`) or `""` when the function has no return annotation.
/// Always returns `Some`; an undeclared type or return is an empty string, not a
/// missing signature. Panic-free: unreadable text degrades to an empty string.
fn python_signature(node: &TsNode, source: &[u8]) -> Option<Signature> {
    let mut params = Vec::new();
    if let Some(parameters) = node.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for child in parameters.named_children(&mut cursor) {
            let param = match child.kind() {
                "identifier" => Param {
                    name: node_text(&child, source),
                    param_type: String::new(),
                },
                "typed_parameter" => Param {
                    name: child
                        .named_child(0)
                        .map(|name| node_text(&name, source))
                        .unwrap_or_default(),
                    param_type: child
                        .child_by_field_name("type")
                        .map(|ty| node_text(&ty, source))
                        .unwrap_or_default(),
                },
                _ => continue,
            };
            if params.is_empty() && (param.name == "self" || param.name == "cls") {
                continue;
            }
            params.push(param);
        }
    }
    let returns = node
        .child_by_field_name("return_type")
        .map(|ty| node_text(&ty, source))
        .unwrap_or_default();
    Some(Signature { params, returns })
}

/// Reads a Python function's docstring: the first body statement when it is an
/// `expression_statement` wrapping a `string`, returning that string's
/// `string_content` child text trimmed.
///
/// Reading `string_content` (rather than hand-stripping quotes) covers `"`,
/// `'`, `"""`, `'''`, and `r`/`b`/`f`-prefixed strings uniformly. Returns `None`
/// when the function has no body, the first statement is not a bare string, or
/// the string carries no `string_content` (e.g. an empty `""`).
fn python_doc(node: &TsNode, source: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let string_node = first.named_child(0)?;
    if string_node.kind() != "string" {
        return None;
    }
    let mut cursor = string_node.walk();
    let content = string_node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "string_content")?;
    content
        .utf8_text(source)
        .ok()
        .map(|text| text.trim().to_string())
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

/// Parses TypeScript `source` at repo-relative `path` into its structural graph.
///
/// Emits a `file` node, one `function` node per top-level `function_declaration`
/// (parented to the file, `meta.language = "typescript"`, `docs` set to the
/// function's preceding JSDoc block via [`typescript_doc`] when present, and
/// `signature` set to its typed params and return type via [`typescript_signature`]),
/// and one
/// `variable` node per single-identifier `variable_declarator` inside a function
/// body (covering
/// `const`/`let` via `lexical_declaration`, parented to its nearest enclosing
/// function), each joined by a `contains` edge. Class methods and arrow functions
/// are deferred to a later phase. Panic-free: malformed input yields the recovered
/// nodes plus a `file` node with status [`NodeStatus::Error`] when the parse tree
/// reports a syntax error.
///
/// Crate-private: callers reach the TypeScript path through the
/// extension-dispatching [`super::parse_source`].
pub(crate) fn parse_typescript(path: &str, source: &str) -> ParsedFile {
    extract(path, source, &typescript_config())
}

/// Returns the [`LanguageConfig`] for TypeScript (`function_declaration` functions,
/// single-identifier `variable_declarator` locals).
fn typescript_config() -> LanguageConfig {
    LanguageConfig {
        language: "typescript",
        grammar: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        function_kinds: &["function_declaration"],
        binding_kinds: &["variable_declarator"],
        binding_name: typescript_binding_name,
        doc_for: typescript_doc,
        signature_of: typescript_signature,
    }
}

/// Reads a TypeScript `function_declaration`'s [`Signature`].
///
/// Walks the `parameters` child (`formal_parameters`): each `required_parameter`
/// or `optional_parameter` yields a [`Param`] whose `name` is its `pattern` field
/// and whose `param_type` is its `type` field's annotation. Because a
/// `type_annotation` node spans the leading `:` too, the annotation text is read
/// from the annotation's first named child (the bare type), or `""` when the
/// parameter is untyped. `returns` is the function's `return_type` annotation read
/// the same way, or `""` when there is no return annotation. Always returns
/// `Some`; an undeclared type or return is an empty string, not a missing
/// signature. Panic-free: unreadable text degrades to an empty string.
fn typescript_signature(node: &TsNode, source: &[u8]) -> Option<Signature> {
    let mut params = Vec::new();
    if let Some(parameters) = node.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for child in parameters.named_children(&mut cursor) {
            if matches!(child.kind(), "required_parameter" | "optional_parameter") {
                params.push(Param {
                    name: child
                        .child_by_field_name("pattern")
                        .map(|pat| node_text(&pat, source))
                        .unwrap_or_default(),
                    param_type: child
                        .child_by_field_name("type")
                        .map(|ann| annotation_type_text(&ann, source))
                        .unwrap_or_default(),
                });
            }
        }
    }
    let returns = node
        .child_by_field_name("return_type")
        .map(|ann| annotation_type_text(&ann, source))
        .unwrap_or_default();
    Some(Signature { params, returns })
}

/// Reads the bare type text out of a TypeScript `type_annotation` node.
///
/// A `type_annotation` spans the leading `:` plus the type, so the annotated type
/// is its first named child; this returns that child's text (e.g. `number` for
/// `: number`), or `""` when the annotation carries no type child.
fn annotation_type_text(annotation: &TsNode, source: &[u8]) -> String {
    annotation
        .named_child(0)
        .map(|ty| node_text(&ty, source))
        .unwrap_or_default()
}

/// Reads a TypeScript function's JSDoc: the `function_declaration`'s
/// `prev_sibling()` when it is a `comment` whose text begins with `/**`.
///
/// Strips the leading `/**`, trailing `*/`, and each line's leading `* ` (or a
/// bare `*`), then joins the lines with `\n` and trims. Returns `None` when there
/// is no preceding sibling, it is not a comment, or it is not a `/**` block (a
/// line `//` or plain `/* */` comment is not JSDoc).
fn typescript_doc(node: &TsNode, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() != "comment" {
        return None;
    }
    let text = prev.utf8_text(source).ok()?;
    let inner = text.trim().strip_prefix("/**")?;
    let inner = inner.strip_suffix("*/").unwrap_or(inner);
    let joined = inner
        .lines()
        .map(strip_jsdoc_line)
        .collect::<Vec<_>>()
        .join("\n");
    Some(joined.trim().to_string())
}

/// Strips a single JSDoc line's leading whitespace and `* `/`*` decoration,
/// leaving the line's content.
fn strip_jsdoc_line(line: &str) -> &str {
    let line = line.trim_start();
    match line.strip_prefix("* ") {
        Some(rest) => rest,
        None => line.strip_prefix('*').unwrap_or(line),
    }
}

/// Reads a TypeScript `variable_declarator` name when its `name` field is a single
/// `identifier`; returns `None` for array/object destructuring patterns (deferred).
fn typescript_binding_name(node: &TsNode, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" {
        return None;
    }
    name.utf8_text(source).ok().map(str::to_string)
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
/// the file, its `docs` read via [`LanguageConfig::doc_for`] and its `signature`
/// via [`LanguageConfig::signature_of`]) and recurses with
/// itself as the new `current_fn`, so a binding is attributed to its **nearest**
/// enclosing function (an inner function's locals never leak to an outer one). A
/// `binding_kinds` child emits a `variable` node (parented to `current_fn`, `docs`
/// always `None`) only when it is inside a function and yields a single-identifier
/// name via [`LanguageConfig::binding_name`].
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
                let docs = (config.doc_for)(&child, source);
                let signature = (config.signature_of)(&child, source);
                push_node(
                    nodes,
                    &fn_id,
                    NodeType::Function,
                    &name,
                    file_id,
                    config,
                    path,
                    &child,
                    docs,
                    signature,
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
                    None,
                    None,
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
///
/// `docs` carries the node's extracted documentation: a function's Python
/// docstring / TypeScript JSDoc when present, and `None` for variable nodes
/// (whose doc extraction is deferred per the Phase-3 doc-scope note). `signature`
/// carries a function's extracted [`Signature`] (typed params plus return type)
/// and is always `None` for variable nodes.
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
    docs: Option<String>,
    signature: Option<Signature>,
) {
    nodes.push(Node {
        id: id.to_string(),
        node_type,
        label: label.to_string(),
        parent_id: Some(parent_id.to_string()),
        child_ids: Vec::new(),
        status: NodeStatus::Unknown,
        docs,
        signature,
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

/// Reads `node`'s UTF-8 source text, degrading to an empty string when the span is
/// not valid UTF-8 (keeps signature extraction panic-free).
fn node_text(node: &TsNode, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or_default().to_string()
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
    fn ts_two_function_declarations_yield_file_and_exactly_two_function_nodes() {
        let parsed = parse_typescript("b.ts", "function foo() {}\nfunction bar() {}\n");
        assert!(ids(&parsed).contains(&"file:b.ts"), "missing file node");
        let fns = function_nodes(&parsed);
        assert_eq!(fns.len(), 2, "expected exactly two function nodes: {fns:?}");
        for id in ["fn:b.ts:foo", "fn:b.ts:bar"] {
            let node = fns
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("missing function node {id}"));
            assert_eq!(node.parent_id.as_deref(), Some("file:b.ts"));
            assert!(
                has_contains_edge(&parsed, "file:b.ts", id),
                "missing contains edge file:b.ts -> {id}"
            );
        }
    }

    #[test]
    fn ts_locals_yield_variable_nodes_with_parents_and_edges() {
        let parsed = parse_typescript("b.ts", "function f() { const x = 1; let y = 2; }");
        let vars = variable_nodes(&parsed);
        assert_eq!(
            vars.len(),
            2,
            "expected exactly two variable nodes: {vars:?}"
        );
        for (id, label) in [("var:b.ts:f:x", "x"), ("var:b.ts:f:y", "y")] {
            let node = vars
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("missing variable node {id}"));
            assert_eq!(node.label, label);
            assert_eq!(node.parent_id.as_deref(), Some("fn:b.ts:f"));
            assert!(
                has_contains_edge(&parsed, "fn:b.ts:f", id),
                "missing contains edge fn:b.ts:f -> {id}"
            );
        }
    }

    #[test]
    fn ts_nodes_carry_language_and_one_based_range() {
        let parsed = parse_typescript("b.ts", "function f() { const x = 1; }");
        let scoped: Vec<&Node> = function_nodes(&parsed)
            .into_iter()
            .chain(variable_nodes(&parsed))
            .collect();
        assert!(!scoped.is_empty(), "expected function/variable nodes");
        for node in scoped {
            let meta = node.meta.as_ref().expect("node has meta");
            assert_eq!(meta.language.as_deref(), Some("typescript"));
            let range = meta.range.expect("node has a range");
            assert!(range.start_line > 0, "startLine must be 1-based: {range:?}");
        }
    }

    #[test]
    fn malformed_typescript_does_not_panic_and_marks_file_error() {
        let result = std::panic::catch_unwind(|| parse_typescript("b.ts", "function ("));
        let parsed = result.expect("parse_typescript must not panic on malformed input");
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:b.ts")
            .expect("file node present");
        assert_eq!(file.node_type, NodeType::File);
        assert_eq!(file.status, NodeStatus::Error);
    }

    #[test]
    fn ts_module_level_const_is_not_attributed_to_any_function() {
        let parsed = parse_typescript("b.ts", "const z = 1;\nfunction f() { const x = 1; }");
        let vars: Vec<&str> = variable_nodes(&parsed)
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(
            vars.contains(&"var:b.ts:f:x"),
            "missing fn-local x: {vars:?}"
        );
        assert!(
            !vars.iter().any(|id| id.ends_with(":z")),
            "module-level z must not be attributed to a function: {vars:?}"
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

    #[test]
    fn python_docstring_populates_function_docs() {
        let parsed = parse_python(
            "a.py",
            "def f():\n    \"\"\"Does a thing.\"\"\"\n    pass\n",
        );
        let f = function_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "fn:a.py:f")
            .expect("missing fn:a.py:f");
        assert_eq!(f.docs.as_deref(), Some("Does a thing."));
    }

    #[test]
    fn python_function_without_docstring_has_none_docs() {
        let parsed = parse_python("a.py", "def g():\n    pass\n");
        let g = function_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "fn:a.py:g")
            .expect("missing fn:a.py:g");
        assert_eq!(g.docs, None);
    }

    #[test]
    fn typescript_jsdoc_populates_function_docs() {
        let parsed = parse_typescript("b.ts", "/** Does a thing. */\nfunction f() {}");
        let f = function_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "fn:b.ts:f")
            .expect("missing fn:b.ts:f");
        assert_eq!(f.docs.as_deref(), Some("Does a thing."));
    }

    #[test]
    fn typescript_function_without_jsdoc_has_none_docs() {
        let parsed = parse_typescript("b.ts", "function g() {}");
        let g = function_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "fn:b.ts:g")
            .expect("missing fn:b.ts:g");
        assert_eq!(g.docs, None);
    }

    #[test]
    fn typescript_multiline_jsdoc_strips_per_line_decoration() {
        let parsed = parse_typescript(
            "b.ts",
            "/**\n * Line one\n * Line two\n */\nfunction f() {}",
        );
        let f = function_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "fn:b.ts:f")
            .expect("missing fn:b.ts:f");
        assert_eq!(f.docs.as_deref(), Some("Line one\nLine two"));
    }

    fn signature_of(parsed: &ParsedFile, id: &str) -> crate::wire::Signature {
        function_nodes(parsed)
            .into_iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("missing function node {id}"))
            .signature
            .clone()
            .unwrap_or_else(|| panic!("function {id} has no signature"))
    }

    #[test]
    fn python_signature_extracts_typed_params_and_return() {
        let parsed = parse_python(
            "a.py",
            "def add(a: int, b: int) -> int:\n    return a + b\n",
        );
        let sig = signature_of(&parsed, "fn:a.py:add");
        assert_eq!(
            sig.params,
            vec![
                crate::wire::Param {
                    name: "a".to_string(),
                    param_type: "int".to_string(),
                },
                crate::wire::Param {
                    name: "b".to_string(),
                    param_type: "int".to_string(),
                },
            ]
        );
        assert_eq!(sig.returns, "int");
    }

    #[test]
    fn python_untyped_params_have_empty_type_and_signature_is_some() {
        let parsed = parse_python("a.py", "def f(x):\n    pass\n");
        let sig = signature_of(&parsed, "fn:a.py:f");
        assert_eq!(
            sig.params,
            vec![crate::wire::Param {
                name: "x".to_string(),
                param_type: String::new(),
            }]
        );
        assert_eq!(sig.returns, "");
    }

    #[test]
    fn python_signature_skips_unsupported_param_kinds() {
        let parsed = parse_python("a.py", "def f(a, *args, b: int):\n    pass\n");
        let sig = signature_of(&parsed, "fn:a.py:f");
        assert_eq!(
            sig.params,
            vec![
                crate::wire::Param {
                    name: "a".to_string(),
                    param_type: String::new(),
                },
                crate::wire::Param {
                    name: "b".to_string(),
                    param_type: "int".to_string(),
                },
            ],
            "*args (list_splat_pattern) must be skipped: {:?}",
            sig.params
        );
    }

    #[test]
    fn python_method_signature_excludes_leading_self() {
        let parsed = parse_python(
            "a.py",
            "class C:\n    def m(self, a: int) -> bool:\n        return True\n",
        );
        let sig = signature_of(&parsed, "fn:a.py:m");
        assert_eq!(
            sig.params,
            vec![crate::wire::Param {
                name: "a".to_string(),
                param_type: "int".to_string(),
            }]
        );
        assert_eq!(sig.returns, "bool");
    }

    #[test]
    fn typescript_signature_extracts_typed_params_and_return() {
        let parsed = parse_typescript(
            "b.ts",
            "function add(a: number, b: number): number { return a + b; }",
        );
        let sig = signature_of(&parsed, "fn:b.ts:add");
        assert_eq!(
            sig.params,
            vec![
                crate::wire::Param {
                    name: "a".to_string(),
                    param_type: "number".to_string(),
                },
                crate::wire::Param {
                    name: "b".to_string(),
                    param_type: "number".to_string(),
                },
            ]
        );
        assert_eq!(sig.returns, "number");
    }

    #[test]
    fn typescript_untyped_params_have_empty_type_and_signature_is_some() {
        let parsed = parse_typescript("b.ts", "function g(x) {}");
        let sig = signature_of(&parsed, "fn:b.ts:g");
        assert_eq!(
            sig.params,
            vec![crate::wire::Param {
                name: "x".to_string(),
                param_type: String::new(),
            }]
        );
        assert_eq!(sig.returns, "");
    }

    #[test]
    fn variable_nodes_keep_none_signature() {
        let parsed = parse_python("a.py", "def f():\n    x = 1\n");
        let var = variable_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "var:a.py:f:x")
            .expect("missing var:a.py:f:x");
        assert_eq!(var.signature, None);
    }

    #[test]
    fn python_valid_function_before_malformed_still_recovers_sibling() {
        // `good` is declared FIRST so the trailing ERROR region cannot swallow it;
        // tree-sitter keeps the valid sibling live while flagging the file Error.
        // Locks the documented contrast with the all-or-nothing `syn` path.
        let parsed = parse_python("x.py", "def good():\n    pass\n\ndef (:\n");
        assert!(
            ids(&parsed).contains(&"fn:x.py:good"),
            "valid sibling must survive a later syntax error: {:?}",
            ids(&parsed)
        );
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:x.py")
            .expect("file node present");
        assert_eq!(
            file.status,
            NodeStatus::Error,
            "a syntax error must flag the file node Error"
        );
    }

    #[test]
    fn typescript_valid_function_before_malformed_still_recovers_sibling() {
        // The TypeScript twin: a valid `good` first, a broken declaration after.
        let parsed = parse_typescript("x.ts", "function good() {}\nfunction (");
        assert!(
            ids(&parsed).contains(&"fn:x.ts:good"),
            "valid sibling must survive a later syntax error: {:?}",
            ids(&parsed)
        );
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:x.ts")
            .expect("file node present");
        assert_eq!(
            file.status,
            NodeStatus::Error,
            "a syntax error must flag the file node Error"
        );
    }
}
