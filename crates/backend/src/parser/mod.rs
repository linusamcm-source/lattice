//! Lowers a single source file to its structural CLV graph contribution.
//!
//! [`parse_source`] is the entry point: it dispatches on the file extension to the
//! matching language path — `rs` → [`parse_rust_source`] (`syn`), `py` →
//! [`parse_python`] and `ts` → [`parse_typescript`] (both `tree-sitter`, in the
//! private [`treesitter`] submodule). Each language path emits the **same** file →
//! function → variable node/edge model (the `tree-sitter` paths tag
//! `meta.language = "python"` / `"typescript"`). Any other extension yields a bare
//! `file` node (status [`NodeStatus::Unknown`], no children) so the file still
//! appears in the graph without an extracted interior. Every path produces a
//! [`ParsedFile`] and recovers panic-free from syntax errors.
//!
//! [`parse_rust_source`] uses `syn` to parse one file into a [`ParsedFile`]: a
//! `file` [`Node`] plus one `function` node per free `fn` and per method declared
//! in an `impl`/`trait` block, and — for each function with a body — one
//! `variable` node per simple `let`-bound identifier in that body. Nodes are
//! joined by `contains` [`Edge`]s (file → function, function → variable). Each
//! Rust function body additionally derives **non-`contains`** edges
//! ([`push_call_and_dataflow_edges`]): a control-flow `calls` edge per callee
//! that resolves intra-file by bare name to a same-file `function` node, plus the
//! data-flow dual `data_flows_from` / `param_source` for the nested-call
//! `outer(inner())` and single `let`-binding `let v = inner(); outer(v)` patterns.
//! These use the kind-qualified [`crate::wire::typed_edge_id`] so they never
//! collide with a `calls` edge on the same ordered pair. Ids come
//! from the §A.1 helpers ([`node_id`]/[`edge_id`]) so elements keep their identity
//! across runs, and every node carries a [`Meta::range`] sourced from the item's
//! span (`proc-macro2`'s `span-locations` feature is required, else spans report
//! line/col `0`). Documentation is surfaced via [`extract_docs`]: the `file` node
//! carries the module-level inner doc (`//!`) and each `function` node its outer
//! doc comments (`///` / `#[doc = "..."]`) in `docs`, while `variable` nodes carry
//! none. Each `function` node also carries its extracted [`crate::wire::Signature`]
//! (typed params with `self` receivers excluded, plus the rendered return type)
//! via [`build_signature`]; `file` and `variable` nodes carry no signature.
//! Variable bindings are deduplicated by name with the latest shadowing
//! binding winning, so the contribution holds no duplicate ids or edges.
//!
//! Per `SPEC.md` §6/§11.1 the parser is **panic-free**: malformed input never
//! aborts. The *granularity* of recovery, however, differs by language, and the
//! difference is a deliberate, documented limitation:
//!
//! - **Rust (`syn`) is all-or-nothing.** `syn::parse_file` either yields a whole
//!   AST or fails outright, so on *any* syntax error [`parse_rust_source`] returns
//!   a [`ParsedFile`] holding **only** the file node with [`NodeStatus::Error`] —
//!   there is no partial tree, and every sibling item (even a syntactically valid
//!   one beside the broken one) is discarded.
//! - **Python / TypeScript (`tree-sitter`) recover siblings.** The shared
//!   [`treesitter`] walk emits every `function`/`variable` it can still read from
//!   the partial parse tree and flags the **file** node [`NodeStatus::Error`] when
//!   the tree reports a syntax error. Valid siblings stay live, so a function
//!   declared *before* a broken one is still extracted. The mark is coarse
//!   (file-level, not the individual offending node); fully marking the offending
//!   node — and closing the `syn` all-or-nothing gap — is a future refinement, not
//!   done here, so `SPEC.md` §11.1's "offending node marked `error`" remains a
//!   file-level approximation on both paths.
//!
//! Either way a downstream graph still receives a well-formed contribution. The
//! caller supplies an already repo-relative `path` (normalisation is a separate
//! concern).

use std::collections::{HashMap, HashSet};

use proc_macro2::Span;
use quote::ToTokens;
use syn::visit::Visit;

use crate::wire::{
    derive_child_ids, edge_id, node_id, typed_edge_id, Edge, EdgeKind, Meta, Node, NodeStatus,
    NodeType, Param, Range, Signature,
};

mod treesitter;

use treesitter::{parse_python, parse_typescript};

/// One Rust file's structural contribution to the CLV graph.
///
/// Holds the [`Node`]s (one `file` node, its `function` children, and each
/// function's `variable` children) and the [`Edge`]s among them, as produced by
/// [`parse_rust_source`]: the structural `contains` edges (file → function,
/// function → variable) plus the derived `calls` / `param_source` /
/// `data_flows_from` edges between same-file functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// The file node, its function children, and each function's variable children.
    pub nodes: Vec<Node>,
    /// `contains` edges (file → function, function → variable) plus the derived
    /// `calls` / `param_source` / `data_flows_from` edges between functions.
    pub edges: Vec<Edge>,
}

/// Parses Rust `source` at repo-relative `path` into its structural graph.
///
/// Returns a [`ParsedFile`] with a `file` node (label = the path's basename,
/// status [`NodeStatus::Unknown`], `docs` set to the module-level inner doc
/// (`//!`) via [`extract_docs`] when present) and one `function` node per free
/// `fn` and per `impl`/`trait` method, each parented to the file with a `contains`
/// edge, a [`Meta::range`] filled from the item's span, `docs` set to the
/// item's outer doc comments (`///` / `#[doc = "..."]`), and `signature` set to
/// the function's extracted [`Signature`] (typed params, `self` excluded, plus the
/// rendered return type) via [`build_signature`]. For every function with a
/// body, one `variable` node is emitted per simple `let`-bound identifier (id
/// `node_id(Variable, path, "<fn>:<name>")`), parented to its function with a
/// `contains` edge; shadowed bindings dedupe to the latest by name. Variable nodes
/// carry no docs (`let` bindings have no doc comments). Finally
/// [`push_call_and_dataflow_edges`] appends the file's `calls` and data-flow
/// (`param_source` / `data_flows_from`) edges derived from the function bodies;
/// the node set and `contains` edges are unchanged by this step.
///
/// Recovery (panic-free, **all-or-nothing**): `syn::parse_file` yields either a
/// whole AST or an error, so on any syntax error the returned [`ParsedFile`]
/// contains only the file node with status [`NodeStatus::Error`] and no functions
/// or variables — even a syntactically valid sibling item beside the broken one is
/// discarded. This is the documented language asymmetry: the `tree-sitter` paths
/// ([`parse_python`] / [`parse_typescript`]) instead keep valid siblings live under
/// a file-level error (see the module doc).
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

    let mut file = file_node(file_id.clone(), label, NodeStatus::Unknown);
    file.docs = extract_docs(&ast.attrs);
    let mut nodes = vec![file];
    let mut edges = Vec::new();

    for item in &ast.items {
        match item {
            syn::Item::Fn(item_fn) => {
                push_function(
                    &mut nodes,
                    &mut edges,
                    &file_id,
                    path,
                    &item_fn.sig,
                    &item_fn.attrs,
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
                            &method.sig,
                            &method.attrs,
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
                            &method.sig,
                            &method.attrs,
                            method.default.as_ref(),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    // Phase 4: derive the file's non-`contains` edges from each function body —
    // control-flow `calls` plus data-flow `param_source` / `data_flows_from` —
    // resolving callees intra-file by bare name. Appended after the structural
    // pass so the node set and `contains` edges stay byte-identical.
    push_call_and_dataflow_edges(&mut edges, path, &ast.items);

    ParsedFile { nodes, edges }
}

/// Parses `source` into its structural graph, dispatching on `path`'s extension.
///
/// Routes by file extension to the matching language path: `rs` →
/// [`parse_rust_source`] (`syn`), `py` → [`parse_python`] and `ts` →
/// [`parse_typescript`] (both `tree-sitter`). Any other extension (or none) is a
/// language Lattice does not parse in Phase 2, so the result is a bare `file` node
/// (status [`NodeStatus::Unknown`], no function/variable children and no edges) —
/// the file still appears in the graph, just without an extracted interior.
/// Panic-free: each delegate recovers from malformed input on its own, with the
/// recovery *asymmetry* documented at the module level — Rust/`syn` is
/// all-or-nothing (a syntax error leaves only the file node with
/// [`NodeStatus::Error`]) while the `tree-sitter` paths keep valid siblings live
/// under a file-level error.
pub fn parse_source(path: &str, source: &str) -> ParsedFile {
    let mut parsed = match std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        Some("rs") => parse_rust_source(path, source),
        Some("py") => parse_python(path, source),
        Some("ts") => parse_typescript(path, source),
        _ => {
            let file_id = node_id(NodeType::File, path, "");
            let label = std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string();
            ParsedFile {
                nodes: vec![file_node(file_id, label, NodeStatus::Unknown)],
                edges: Vec::new(),
            }
        }
    };
    populate_child_ids(&mut parsed);
    parsed
}

/// Fills each node's `child_ids` from the file's `contains` edges.
///
/// The lazy snapshot ships parent nodes without their child subtrees, but the
/// client needs to know a node *has* children to render an expand affordance (the
/// subtree itself is fetched on `expand`). This derives `child_ids` from the
/// `contains` edges so a `file` node lists its `function` children and a
/// `function` node lists its `variable` children.
///
/// The derivation goes through the shared [`derive_child_ids`] helper, which sorts
/// each `child_ids` **canonically by child id**. Using the same helper as the
/// crash-rebuild [`crate::graph::Graph::from_records`] guarantees a freshly parsed node
/// and a node rebuilt from persisted records are **byte-equal**, so a reparse after a
/// warm start is a no-op regardless of the loaded edge order.
fn populate_child_ids(parsed: &mut ParsedFile) {
    let mut children = derive_child_ids(&parsed.edges);
    for node in &mut parsed.nodes {
        if let Some(ids) = children.remove(&node.id) {
            node.child_ids = ids;
        }
    }
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

/// Appends a `function` node for `sig.ident` plus its `contains` edge from the
/// file, then extracts that function's `let`-bound variables when `body` is present.
///
/// The function node id is `node_id(Function, path, name)`, the label is the
/// function name, the parent is `file_id`, the range is taken from the
/// identifier's span (1-based line, 0-based column) via [`span_range`], and `docs`
/// is the item's outer doc comments (`///` / `#[doc = "..."]`) extracted from
/// `attrs` via [`extract_docs`] (`None` when undocumented). `signature` is always
/// `Some`, derived from `sig` via [`build_signature`]: its typed parameters
/// (`self` receivers excluded) and its rendered return type (`""` for a unit
/// return). When `body` is `Some` (free fns and `impl`/`trait` methods that have a
/// block) its simple `let` bindings are lowered to `variable` children via
/// [`push_variables`]; a trait method declared without a body (`body == None`)
/// contributes no variables.
fn push_function(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    file_id: &str,
    path: &str,
    sig: &syn::Signature,
    attrs: &[syn::Attribute],
    body: Option<&syn::Block>,
) {
    let ident = &sig.ident;
    let name = ident.to_string();
    let fn_id = node_id(NodeType::Function, path, &name);
    nodes.push(Node {
        id: fn_id.clone(),
        node_type: NodeType::Function,
        label: name.clone(),
        parent_id: Some(file_id.to_string()),
        child_ids: Vec::new(),
        status: NodeStatus::Unknown,
        docs: extract_docs(attrs),
        signature: Some(build_signature(sig)),
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

/// Derives the file's non-`contains` edges from every Rust function body.
///
/// Resolution is deliberately **intra-file, same-language, by bare callee name**:
/// a callee resolves only when it is a single-segment path naming a `function`
/// node declared in the same file (collected up-front by
/// [`collect_function_names`] so a call to a function defined later still
/// resolves). There is no cross-file, import, type, or trait resolution; method
/// calls (`x.foo()`) and qualified paths (`T::foo()`) never resolve. Unresolved
/// callees (external / std / imported) are skipped silently. For each function
/// body (free `fn`, `impl` method, or `trait` method with a default body) a
/// [`BodyEdgeVisitor`] emits, deduplicated:
/// - a `calls` edge `caller --calls--> callee` per resolved callee;
/// - the dual data-flow pair `inner --data_flows_from--> outer` and
///   `outer --param_source--> inner` for the two static patterns nested call
///   `outer(inner())` and single `let`-binding indirection
///   `let v = inner(); outer(v)`.
///
/// All edges use [`typed_edge_id`] so a `calls` and a `data_flows_from` edge on
/// the same ordered pair keep distinct ids. Out of scope (not derived, never a
/// panic): method-call chains, aliasing / reassignment, multi-hop bindings, and
/// tuple destructuring.
fn push_call_and_dataflow_edges(edges: &mut Vec<Edge>, path: &str, items: &[syn::Item]) {
    let fn_names = collect_function_names(items);
    for item in items {
        match item {
            syn::Item::Fn(item_fn) => {
                push_body_edges(
                    edges,
                    path,
                    &item_fn.sig.ident.to_string(),
                    &item_fn.block,
                    &fn_names,
                );
            }
            syn::Item::Impl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let syn::ImplItem::Fn(method) = impl_item {
                        push_body_edges(
                            edges,
                            path,
                            &method.sig.ident.to_string(),
                            &method.block,
                            &fn_names,
                        );
                    }
                }
            }
            syn::Item::Trait(item_trait) => {
                for trait_item in &item_trait.items {
                    if let syn::TraitItem::Fn(method) = trait_item {
                        if let Some(block) = &method.default {
                            push_body_edges(
                                edges,
                                path,
                                &method.sig.ident.to_string(),
                                block,
                                &fn_names,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Collects the names of every `function` node the file declares.
///
/// Mirrors the item walk in [`parse_rust_source`] (free `fn`s, `impl` methods,
/// `trait` methods) so the returned set is exactly the bare names a callee can
/// resolve against in [`push_call_and_dataflow_edges`]. Names are deduplicated by
/// the `HashSet`; same-named methods across `impl` blocks collapse to one entry,
/// matching the existing name-based `function` node id scheme.
fn collect_function_names(items: &[syn::Item]) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in items {
        match item {
            syn::Item::Fn(item_fn) => {
                names.insert(item_fn.sig.ident.to_string());
            }
            syn::Item::Impl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let syn::ImplItem::Fn(method) = impl_item {
                        names.insert(method.sig.ident.to_string());
                    }
                }
            }
            syn::Item::Trait(item_trait) => {
                for trait_item in &item_trait.items {
                    if let syn::TraitItem::Fn(method) = trait_item {
                        names.insert(method.sig.ident.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// Walks one function `block` and appends its derived call / data-flow edges.
///
/// Drives a [`BodyEdgeVisitor`] over the block via `syn`'s [`Visit`] traversal so
/// every (possibly nested) call expression and `let` binding is visited
/// panic-free; the visitor owns the per-body dedup state and resolves callees
/// against `fn_names`. `caller_name` is the owning function's bare name (its
/// `function` node is `node_id(Function, path, caller_name)`).
fn push_body_edges(
    edges: &mut Vec<Edge>,
    path: &str,
    caller_name: &str,
    block: &syn::Block,
    fn_names: &HashSet<String>,
) {
    let mut visitor = BodyEdgeVisitor {
        path,
        caller_id: node_id(NodeType::Function, path, caller_name),
        fn_names,
        bindings: HashMap::new(),
        calls_seen: HashSet::new(),
        dataflow_seen: HashSet::new(),
        edges,
    };
    syn::visit::visit_block(&mut visitor, block);
}

/// Accumulates one function body's derived `calls` / `param_source` /
/// `data_flows_from` edges during a `syn` [`Visit`] walk.
///
/// Created per function body by [`push_body_edges`]; it resolves bare callee
/// names against `fn_names`, tracks `let v = inner()` bindings, and deduplicates
/// both the `calls` edges (per callee) and the data-flow dependencies (per
/// `(inner, outer)` pair) so a body produces each edge at most once.
struct BodyEdgeVisitor<'a> {
    /// Repo-relative path of the file being lowered (for callee node ids).
    path: &'a str,
    /// `function` node id of the body's owning function (every edge's source/target base).
    caller_id: String,
    /// Bare names of same-file functions a callee may resolve to.
    fn_names: &'a HashSet<String>,
    /// `let`-bound variable name → the same-file callee it was initialised from.
    bindings: HashMap<String, String>,
    /// Callee names already emitted as a `calls` edge from this body.
    calls_seen: HashSet<String>,
    /// `(inner, outer)` dependencies already emitted as a data-flow dual.
    dataflow_seen: HashSet<(String, String)>,
    /// The file's edge accumulator, appended to in place.
    edges: &'a mut Vec<Edge>,
}

impl BodyEdgeVisitor<'_> {
    /// Resolves a call's callee to a same-file function's bare name.
    ///
    /// Returns the name only when `func` is a single-segment, unqualified path
    /// that is present in `fn_names`; qualified paths, method receivers, and
    /// external / std names yield `None` (skipped silently).
    fn resolve_callee(&self, func: &syn::Expr) -> Option<String> {
        let name = path_single_ident(func)?;
        self.fn_names.contains(&name).then_some(name)
    }

    /// Appends a deduplicated `caller --calls--> callee` edge for `callee`.
    fn push_call(&mut self, callee: &str) {
        if !self.calls_seen.insert(callee.to_string()) {
            return;
        }
        let target = node_id(NodeType::Function, self.path, callee);
        self.edges.push(Edge {
            id: typed_edge_id(&self.caller_id, &target, EdgeKind::Calls),
            source: self.caller_id.clone(),
            target,
            kind: EdgeKind::Calls,
            hot: false,
        });
    }

    /// Appends the deduplicated dual data-flow edges for `inner`'s return value
    /// flowing into `outer`'s parameter.
    ///
    /// Emits both `inner --data_flows_from--> outer` and
    /// `outer --param_source--> inner` (`DATA_MODEL.md` §A.3), at most once per
    /// `(inner, outer)` pair per body.
    fn push_data_flow(&mut self, inner: &str, outer: &str) {
        if !self
            .dataflow_seen
            .insert((inner.to_string(), outer.to_string()))
        {
            return;
        }
        let inner_id = node_id(NodeType::Function, self.path, inner);
        let outer_id = node_id(NodeType::Function, self.path, outer);
        self.edges.push(Edge {
            id: typed_edge_id(&inner_id, &outer_id, EdgeKind::DataFlowsFrom),
            source: inner_id.clone(),
            target: outer_id.clone(),
            kind: EdgeKind::DataFlowsFrom,
            hot: false,
        });
        self.edges.push(Edge {
            id: typed_edge_id(&outer_id, &inner_id, EdgeKind::ParamSource),
            source: outer_id,
            target: inner_id,
            kind: EdgeKind::ParamSource,
            hot: false,
        });
    }
}

impl<'ast> Visit<'ast> for BodyEdgeVisitor<'_> {
    /// Records a `let v = inner();` binding (`v` → `inner`) when the initialiser
    /// is a call resolving to a same-file function, then continues the walk so
    /// the initialiser's own call still yields its `calls` edge.
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(var) = pat_binding_ident(&local.pat) {
            // Resolve the initialiser to a same-file callee, if it is one.
            let resolved = local.init.as_ref().and_then(|init| {
                if let syn::Expr::Call(call) = init.expr.as_ref() {
                    self.resolve_callee(&call.func)
                } else {
                    None
                }
            });
            // A `let` always supersedes a prior binding of the same name: insert
            // when the initialiser resolves to a call, otherwise clear any stale
            // mapping so a shadow (`let t = inner(); let t = other(); outer(t);`)
            // cannot emit a spurious data-flow edge from the shadowed call.
            match resolved {
                Some(callee) => {
                    self.bindings.insert(var, callee);
                }
                None => {
                    self.bindings.remove(&var);
                }
            }
        }
        syn::visit::visit_local(self, local);
    }

    /// Emits the `calls` edge for a resolved callee and any nested-call or
    /// `let`-binding data-flow dependency carried by its arguments, then recurses
    /// so nested calls (e.g. the `inner()` in `outer(inner())`) are also visited.
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Some(outer) = self.resolve_callee(&call.func) {
            self.push_call(&outer);
            for arg in &call.args {
                if let syn::Expr::Call(inner_call) = arg {
                    // Nested call: `outer(inner())`.
                    if let Some(inner) = self.resolve_callee(&inner_call.func) {
                        self.push_data_flow(&inner, &outer);
                    }
                } else if let Some(var) = path_single_ident(arg) {
                    // Let-binding indirection: `let v = inner(); outer(v)`.
                    if let Some(inner) = self.bindings.get(&var).cloned() {
                        self.push_data_flow(&inner, &outer);
                    }
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

/// Returns the identifier of a single-segment, unqualified path expression.
///
/// Yields `Some(name)` for a bare path like `foo` or `v` (used both for callee
/// names and for plain variable arguments) and `None` for qualified paths
/// (`T::foo`), paths with a `qself`, or any non-path expression.
fn path_single_ident(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Path(expr_path) = expr {
        if expr_path.qself.is_none() && expr_path.path.segments.len() == 1 {
            return Some(expr_path.path.segments[0].ident.to_string());
        }
    }
    None
}

/// Returns the bound identifier of a simple (optionally type-ascribed) `let`
/// pattern, or `None` for destructuring / wildcard patterns.
///
/// Handles `let v = ..` ([`syn::Pat::Ident`] without a sub-pattern) and
/// `let v: T = ..` ([`syn::Pat::Type`]); tuple / struct destructuring and `_`
/// wildcards yield `None`, so they are not tracked as data-flow bindings.
fn pat_binding_ident(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(pat_ident) if pat_ident.subpat.is_none() => {
            Some(pat_ident.ident.to_string())
        }
        syn::Pat::Type(pat_type) => pat_binding_ident(&pat_type.pat),
        _ => None,
    }
}

/// Lowers a `syn` [`syn::Signature`] to the CLV [`Signature`] (params + return).
///
/// Each [`syn::FnArg::Typed`] input becomes a [`Param`] whose `name` is the
/// binding pattern and `param_type` is the declared type, both rendered as clean
/// source text via [`render_tokens`]. A [`syn::FnArg::Receiver`] (`self` / `&self`)
/// is skipped — it is not a data parameter. `returns` is the return type rendered
/// the same way, or the empty string for a default (unit) return. A fn with no
/// typed params and a unit return yields `Signature { params: vec![], returns:
/// String::new() }` (the caller still stores it as `Some`).
fn build_signature(sig: &syn::Signature) -> Signature {
    let mut params = Vec::new();
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pat_type) = input {
            params.push(Param {
                name: render_tokens(pat_type.pat.as_ref()),
                param_type: render_tokens(pat_type.ty.as_ref()),
            });
        }
    }
    let returns = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => render_tokens(ty.as_ref()),
    };
    Signature { params, returns }
}

/// Renders any token-bearing `syn` node to whitespace-collapsed source text.
///
/// `proc-macro2` token printing pads every token with a space, so a type like
/// `Vec<T>` round-trips through tokens as `Vec < T >`. This rejoins those pieces,
/// keeping a single space only between two adjacent *word* tokens (so `dyn Trait`
/// survives) and dropping it around punctuation — yielding clean text such as
/// `i32`, `Credentials`, or `Vec<T>`. Used for both parameter names/types and
/// return types in [`build_signature`].
fn render_tokens<T: ToTokens>(node: &T) -> String {
    let raw = node.to_token_stream().to_string();
    let mut out = String::with_capacity(raw.len());
    for piece in raw.split_whitespace() {
        if let (Some(prev), Some(next)) = (out.chars().last(), piece.chars().next()) {
            if is_word_char(prev) && is_word_char(next) {
                out.push(' ');
            }
        }
        out.push_str(piece);
    }
    out
}

/// Reports whether `c` is an identifier character (alphanumeric or `_`).
///
/// Used by [`render_tokens`] to decide when two adjacent token pieces need a
/// separating space (two word characters) versus none (around punctuation).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

/// Extracts the doc-comment text carried by `attrs`, or `None` when there is none.
///
/// Collects each `doc` attribute (`syn` lowers both `///`/`//!` line comments and
/// explicit `#[doc = "..."]` to a [`syn::Meta::NameValue`] whose path is `doc` and
/// whose value is a string literal) in source order, stripping a single leading
/// space per line — `rustfmt` renders `///x` as `#[doc = " x"]` — and joins the
/// lines with `\n`. Works for both outer item docs (`///`) and the inner
/// module-level doc (`//!`) exposed as `syn::File.attrs`. Non-`doc` attributes are
/// ignored. Panic-free: malformed `doc` attributes are skipped rather than
/// unwrapped.
fn extract_docs(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if let syn::Meta::NameValue(name_value) = &attr.meta {
            if !name_value.path.is_ident("doc") {
                continue;
            }
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(text),
                ..
            }) = &name_value.value
            {
                let line = text.value();
                lines.push(line.strip_prefix(' ').unwrap_or(&line).to_string());
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
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
    fn parse_source_routes_by_extension_to_the_right_language() {
        let cases: Vec<(&str, &str, &str)> = vec![
            ("x.rs", "fn f() {}", "fn:x.rs:f"),
            ("x.py", "def f():\n    pass\n", "fn:x.py:f"),
            ("x.ts", "function f() {}", "fn:x.ts:f"),
        ];
        for (path, source, want_fn) in cases {
            let parsed = parse_source(path, source);
            assert!(
                ids(&parsed).contains(&want_fn),
                "{path}: missing {want_fn}, got {:?}",
                ids(&parsed)
            );
        }
    }

    #[test]
    fn parse_source_populates_child_ids_from_contains_edges() {
        let parsed = parse_source("a.rs", "fn f() { let x = 1; }");
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:a.rs")
            .expect("file node");
        assert!(
            file.child_ids.contains(&"fn:a.rs:f".to_string()),
            "file childIds: {:?}",
            file.child_ids
        );
        let func = parsed
            .nodes
            .iter()
            .find(|n| n.id == "fn:a.rs:f")
            .expect("function node");
        assert!(
            func.child_ids.contains(&"var:a.rs:f:x".to_string()),
            "function childIds: {:?}",
            func.child_ids
        );
    }

    #[test]
    fn parse_source_unknown_extension_yields_only_a_file_node() {
        let parsed = parse_source("x.md", "# hi");
        assert_eq!(ids(&parsed), vec!["file:x.md"], "only the file node");
        assert!(
            function_nodes(&parsed).is_empty(),
            "unknown extension must yield no function nodes"
        );
        assert_eq!(parsed.nodes[0].status, NodeStatus::Unknown);
        assert!(parsed.edges.is_empty(), "no edges for a bare file node");
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

    fn function_node<'a>(parsed: &'a ParsedFile, id: &str) -> &'a Node {
        function_nodes(parsed)
            .into_iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("missing function node {id}"))
    }

    #[test]
    fn outer_doc_comment_populates_function_docs() {
        let parsed = parse_rust_source("a.rs", "/// Adds two numbers.\nfn add() {}");
        let func = function_node(&parsed, "fn:a.rs:add");
        assert_eq!(func.docs.as_deref(), Some("Adds two numbers."));
    }

    #[test]
    fn multiline_outer_doc_comments_join_with_newline() {
        let parsed = parse_rust_source("a.rs", "/// line one\n/// line two\nfn f() {}");
        let func = function_node(&parsed, "fn:a.rs:f");
        assert_eq!(func.docs.as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn function_without_doc_comment_has_none_docs() {
        let parsed = parse_rust_source("a.rs", "fn bare() {}");
        let func = function_node(&parsed, "fn:a.rs:bare");
        assert_eq!(func.docs, None);
    }

    #[test]
    fn module_inner_doc_populates_file_node_docs() {
        let parsed = parse_rust_source("a.rs", "//! Module docs.\nfn f() {}");
        let file = parsed
            .nodes
            .iter()
            .find(|n| n.id == "file:a.rs")
            .expect("file node");
        let docs = file.docs.as_deref().expect("file node has docs");
        assert!(docs.contains("Module docs."), "file docs: {docs:?}");
    }

    #[test]
    fn doc_edit_re_derives_function_docs() {
        let first = parse_rust_source("a.rs", "/// v1\nfn f() {}");
        assert_eq!(
            function_node(&first, "fn:a.rs:f").docs.as_deref(),
            Some("v1")
        );

        let second = parse_rust_source("a.rs", "/// v2\nfn f() {}");
        assert_eq!(
            function_node(&second, "fn:a.rs:f").docs.as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn function_signature_extracts_typed_params_and_return() {
        let parsed = parse_rust_source("a.rs", "fn add(a: i32, b: i32) -> i32 { a + b }");
        let func = function_node(&parsed, "fn:a.rs:add");
        assert_eq!(
            func.signature,
            Some(crate::wire::Signature {
                params: vec![
                    crate::wire::Param {
                        name: "a".to_string(),
                        param_type: "i32".to_string(),
                    },
                    crate::wire::Param {
                        name: "b".to_string(),
                        param_type: "i32".to_string(),
                    },
                ],
                returns: "i32".to_string(),
            })
        );
    }

    #[test]
    fn function_with_no_params_and_unit_return_has_empty_signature() {
        let parsed = parse_rust_source("a.rs", "fn noop() {}");
        let func = function_node(&parsed, "fn:a.rs:noop");
        assert_eq!(
            func.signature,
            Some(crate::wire::Signature {
                params: vec![],
                returns: String::new(),
            })
        );
    }

    #[test]
    fn method_signature_excludes_self_receiver() {
        let parsed = parse_rust_source(
            "a.rs",
            "struct S; impl S { fn m(&self, x: u8) -> bool { true } }",
        );
        let func = function_node(&parsed, "fn:a.rs:m");
        let sig = func.signature.as_ref().expect("method has a signature");
        assert_eq!(
            sig.params,
            vec![crate::wire::Param {
                name: "x".to_string(),
                param_type: "u8".to_string(),
            }],
            "the &self receiver must be excluded from params"
        );
        assert_eq!(sig.returns, "bool");
    }

    #[test]
    fn signature_param_type_is_re_derived_on_edit() {
        let first = parse_rust_source("a.rs", "fn f(a: i32) {}");
        assert_eq!(
            function_node(&first, "fn:a.rs:f")
                .signature
                .as_ref()
                .expect("first signature")
                .params[0]
                .param_type,
            "i32"
        );

        let second = parse_rust_source("a.rs", "fn f(a: i64) {}");
        assert_eq!(
            function_node(&second, "fn:a.rs:f")
                .signature
                .as_ref()
                .expect("second signature")
                .params[0]
                .param_type,
            "i64"
        );
    }

    #[test]
    fn generic_param_type_is_whitespace_collapsed() {
        let parsed = parse_rust_source("a.rs", "fn f(items: Vec<u8>) {}");
        let sig = function_node(&parsed, "fn:a.rs:f")
            .signature
            .as_ref()
            .expect("signature");
        assert_eq!(sig.params[0].param_type, "Vec<u8>");
    }

    #[test]
    fn variable_nodes_keep_none_signature() {
        let parsed = parse_rust_source("a.rs", "fn f() { let x = 1; }");
        let var = variable_nodes(&parsed)
            .into_iter()
            .find(|n| n.id == "var:a.rs:f:x")
            .expect("variable node");
        assert_eq!(var.signature, None);
    }

    fn typed_edge<'a>(
        parsed: &'a ParsedFile,
        source: &str,
        target: &str,
        kind: EdgeKind,
    ) -> Option<&'a Edge> {
        parsed
            .edges
            .iter()
            .find(|e| e.source == source && e.target == target && e.kind == kind)
    }

    fn kind_count(parsed: &ParsedFile, kind: EdgeKind) -> usize {
        parsed.edges.iter().filter(|e| e.kind == kind).count()
    }

    #[test]
    fn call_expression_yields_calls_edge_and_no_data_flow() {
        let parsed = parse_rust_source("x.rs", "fn a() {}\nfn b() { a(); }");
        let edge = typed_edge(&parsed, "fn:x.rs:b", "fn:x.rs:a", EdgeKind::Calls)
            .expect("expected calls edge b -> a");
        assert_eq!(edge.kind, EdgeKind::Calls);
        assert_eq!(
            kind_count(&parsed, EdgeKind::DataFlowsFrom),
            0,
            "a bare call consumes no value: no data-flow edge"
        );
        assert_eq!(kind_count(&parsed, EdgeKind::ParamSource), 0);
    }

    #[test]
    fn same_pair_calls_and_data_flow_have_distinct_ids() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn y(v: i32) -> i32 { v }\nfn x() -> i32 { y(3) }\nfn m() { y(x()); }",
        );
        let calls = typed_edge(&parsed, "fn:x.rs:x", "fn:x.rs:y", EdgeKind::Calls)
            .expect("expected calls edge x -> y (from x's body)");
        let flow = typed_edge(&parsed, "fn:x.rs:x", "fn:x.rs:y", EdgeKind::DataFlowsFrom)
            .expect("expected data_flows_from edge x -> y (from m's body)");
        assert_ne!(calls.id, flow.id, "kind-qualified ids must differ");
        assert!(calls.id.ends_with(":calls"), "calls id: {}", calls.id);
        assert!(
            flow.id.ends_with(":data_flows_from"),
            "data-flow id: {}",
            flow.id
        );
    }

    #[test]
    fn nested_call_derives_dual_data_flow_and_calls() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { outer(inner()); }",
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:inner",
                "fn:x.rs:outer",
                EdgeKind::DataFlowsFrom
            )
            .is_some(),
            "missing inner --data_flows_from--> outer"
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:outer",
                "fn:x.rs:inner",
                EdgeKind::ParamSource
            )
            .is_some(),
            "missing outer --param_source--> inner"
        );
        assert!(
            typed_edge(&parsed, "fn:x.rs:f", "fn:x.rs:outer", EdgeKind::Calls).is_some(),
            "missing f --calls--> outer"
        );
        assert!(
            typed_edge(&parsed, "fn:x.rs:f", "fn:x.rs:inner", EdgeKind::Calls).is_some(),
            "missing f --calls--> inner"
        );
    }

    #[test]
    fn let_binding_indirection_derives_dual_data_flow() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { let t = inner(); outer(t); }",
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:inner",
                "fn:x.rs:outer",
                EdgeKind::DataFlowsFrom
            )
            .is_some(),
            "missing inner --data_flows_from--> outer"
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:outer",
                "fn:x.rs:inner",
                EdgeKind::ParamSource
            )
            .is_some(),
            "missing outer --param_source--> inner"
        );
    }

    #[test]
    fn typed_let_binding_indirection_still_derives_data_flow() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { let t: i32 = inner(); outer(t); }",
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:inner",
                "fn:x.rs:outer",
                EdgeKind::DataFlowsFrom
            )
            .is_some(),
            "type-ascribed binding must still resolve the data-flow"
        );
    }

    #[test]
    fn shadowed_binding_does_not_emit_stale_data_flow_edge() {
        // `t` is rebound to an unresolved (external) call before being passed to
        // `outer`, so the stale `t -> inner` mapping must be cleared: no spurious
        // inner --data_flows_from--> outer (or the param_source dual) edge.
        let parsed = parse_rust_source(
            "x.rs",
            "fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { let t = inner(); let t = external(); outer(t); }",
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:inner",
                "fn:x.rs:outer",
                EdgeKind::DataFlowsFrom
            )
            .is_none(),
            "shadowed binding must not emit a stale data-flow edge"
        );
        assert!(
            typed_edge(
                &parsed,
                "fn:x.rs:outer",
                "fn:x.rs:inner",
                EdgeKind::ParamSource
            )
            .is_none(),
            "shadowed binding must not emit a stale param_source edge"
        );
    }

    #[test]
    fn unresolved_external_callee_yields_no_call_or_data_flow_edges() {
        let parsed = parse_rust_source("x.rs", "fn b() { external_lib(); }");
        assert_eq!(
            kind_count(&parsed, EdgeKind::Calls),
            0,
            "external callee skipped"
        );
        assert_eq!(kind_count(&parsed, EdgeKind::DataFlowsFrom), 0);
        assert_eq!(kind_count(&parsed, EdgeKind::ParamSource), 0);
    }

    #[test]
    fn malformed_body_recovers_without_derived_edges() {
        let result = std::panic::catch_unwind(|| parse_rust_source("x.rs", "fn b( { a("));
        let parsed = result.expect("parse_rust_source must not panic on malformed input");
        assert_eq!(parsed.nodes.len(), 1, "only the file node on parse error");
        assert_eq!(parsed.nodes[0].node_type, NodeType::File);
        assert_eq!(parsed.nodes[0].status, NodeStatus::Error);
        assert!(
            parsed.edges.is_empty(),
            "no derived edges on malformed input"
        );
    }

    #[test]
    fn repeated_call_to_same_callee_dedupes_to_one_calls_edge() {
        let parsed = parse_rust_source("x.rs", "fn a() {}\nfn b() { a(); a(); }");
        assert_eq!(
            kind_count(&parsed, EdgeKind::Calls),
            1,
            "duplicate calls to the same callee must dedupe to one edge"
        );
    }

    #[test]
    fn repeated_data_flow_dependency_dedupes() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn inner() -> i32 { 1 }\nfn outer(v: i32) {}\nfn f() { outer(inner()); outer(inner()); }",
        );
        assert_eq!(
            kind_count(&parsed, EdgeKind::DataFlowsFrom),
            1,
            "data-flow dedupes"
        );
        assert_eq!(kind_count(&parsed, EdgeKind::ParamSource), 1);
    }

    #[test]
    fn method_calls_and_qualified_paths_are_not_derived() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn a() {}\nfn b() { let (p, q) = (1, 2); let s = String::new(); s.len(); }",
        );
        // `String::new` (two-segment path), `s.len()` (method call), and the tuple
        // destructuring `let (p, q)` resolve to no same-file function, so no
        // call / data-flow edges are derived.
        assert_eq!(kind_count(&parsed, EdgeKind::Calls), 0);
        assert_eq!(kind_count(&parsed, EdgeKind::DataFlowsFrom), 0);
        assert_eq!(kind_count(&parsed, EdgeKind::ParamSource), 0);
    }

    #[test]
    fn impl_method_call_resolves_to_same_file_function() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn helper() {}\nstruct S;\nimpl S { fn run(&self) { helper(); } }",
        );
        assert!(
            typed_edge(&parsed, "fn:x.rs:run", "fn:x.rs:helper", EdgeKind::Calls).is_some(),
            "an impl method body must derive calls edges too"
        );
    }

    #[test]
    fn trait_default_method_body_derives_calls() {
        let parsed = parse_rust_source(
            "x.rs",
            "fn helper() {}\ntrait T { fn run(&self) { helper(); } }",
        );
        assert!(
            typed_edge(&parsed, "fn:x.rs:run", "fn:x.rs:helper", EdgeKind::Calls).is_some(),
            "a trait default-method body must derive calls edges too"
        );
    }

    #[test]
    fn contains_edges_unchanged_when_calls_present() {
        let parsed = parse_rust_source("x.rs", "fn a() {}\nfn b() { a(); }");
        assert!(has_contains_edge(&parsed, "file:x.rs", "fn:x.rs:a"));
        assert!(has_contains_edge(&parsed, "file:x.rs", "fn:x.rs:b"));
        assert_eq!(
            kind_count(&parsed, EdgeKind::Contains),
            2,
            "exactly the two file->function contains edges, unaffected by calls"
        );
    }

    /// Deterministic, dependency-free pseudo-random byte source (a 64-bit LCG) so
    /// the fuzz corpus is fully reproducible — no `rand`, no wall-clock seed.
    fn lcg_next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    /// Builds the deterministic malformed/random corpus exercised by the
    /// panic-freedom tests: a fixed set of hand-picked malformed fragments (across
    /// several languages plus adversarial byte shapes) followed by a bounded,
    /// seeded run of pseudo-random byte vectors made into valid `&str` via lossy
    /// UTF-8 conversion. Identical on every run.
    fn fuzz_corpus() -> Vec<String> {
        let fixed: &[&str] = &[
            "",
            "fn foo( {",
            "fn b( { a(",
            "def (:\n",
            "function (",
            "}}}}{{{{",
            "\"unterminated string",
            "/* unclosed comment",
            "let x = ;;;",
            "🦀🦀🦀 not code 🦀",
            "\0\0\0\0",
            "use use use use",
            "impl impl for for {{",
            "\t\n\r  \n",
            "(((((((((((((((((((",
            "class C: def def def",
            "const = = = =>",
            "/// doc with no item",
            "//! \n//! \n",
            "0xZZZ 1.2.3.4 'a",
        ];
        let mut corpus: Vec<String> = fixed.iter().map(|s| (*s).to_string()).collect();

        let mut state: u64 = 0x5151_5151_5151_5151;
        for _ in 0..64 {
            let len = (lcg_next(&mut state) % 48) as usize;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push((lcg_next(&mut state) & 0xff) as u8);
            }
            corpus.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        corpus
    }

    #[test]
    fn parse_source_never_panics_across_dispatch_for_malformed_corpus() {
        // Every dispatch arm (rs/py/ts + an unknown extension) must survive the
        // whole malformed/random corpus without panicking and still return at
        // least a file node so the file stays visible in the graph.
        let extensions = ["rs", "py", "ts", "md"];
        let corpus = fuzz_corpus();
        for ext in extensions {
            let path = format!("fuzz.{ext}");
            for source in &corpus {
                let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse_source(&path, source)
                }))
                .unwrap_or_else(|_| panic!("parse_source panicked: ext={ext} source={source:?}"));
                assert!(
                    !parsed.nodes.is_empty(),
                    "ext={ext} source={source:?}: expected at least a file node"
                );
                assert_eq!(
                    parsed.nodes[0].node_type,
                    NodeType::File,
                    "ext={ext} source={source:?}: first node must be the file node"
                );
            }
        }
    }

    #[test]
    fn parse_source_unknown_extension_and_empty_source_yield_a_bare_file_node() {
        // An unknown extension is never parsed: any body (including the whole
        // fuzz corpus) yields exactly the bare `file` node, status Unknown.
        for source in fuzz_corpus() {
            let parsed = parse_source("fuzz.unknownext", &source);
            assert_eq!(
                ids(&parsed),
                vec!["file:fuzz.unknownext"],
                "unknown extension must yield only a bare file node"
            );
            assert_eq!(parsed.nodes[0].status, NodeStatus::Unknown);
            assert!(parsed.edges.is_empty(), "bare file node has no edges");
        }
        // Empty source on every parsed extension still yields a file node.
        for ext in ["rs", "py", "ts", "md"] {
            let parsed = parse_source(&format!("empty.{ext}"), "");
            assert!(
                !parsed.nodes.is_empty(),
                "empty {ext} source must still yield a file node"
            );
            assert_eq!(parsed.nodes[0].node_type, NodeType::File);
        }
    }

    #[test]
    fn rust_is_all_or_nothing_a_valid_fn_beside_a_syntax_error_is_discarded() {
        // Locks the documented language asymmetry: unlike the tree-sitter paths
        // (which keep valid siblings live), `syn` is all-or-nothing — one syntax
        // error anywhere discards every sibling, so a perfectly valid `good`
        // beside a broken item leaves ONLY the file node, status Error.
        let parsed = parse_source("x.rs", "fn good() {}\nfn bad( {");
        assert_eq!(
            ids(&parsed),
            vec!["file:x.rs"],
            "syn discards all siblings on any syntax error"
        );
        assert!(
            function_nodes(&parsed).is_empty(),
            "no function nodes survive a syn parse error"
        );
        assert!(
            variable_nodes(&parsed).is_empty(),
            "no variable nodes survive a syn parse error"
        );
        assert_eq!(parsed.nodes[0].status, NodeStatus::Error);
        assert!(parsed.edges.is_empty(), "no edges on a syn parse error");
    }
}
