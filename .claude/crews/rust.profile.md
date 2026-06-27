# Lattice — Rust Stack Profile

> Source of truth for all crew agents. Every claim traces to a verified tool call.
> Verified: `just qg` passes clean (1 passing test, 0 warnings) as of profile creation.

## Language

Rust (edition 2021). Cargo workspace at repo root (`/Users/linus/Development/lattice/Cargo.toml`).
Primary crate: `crates/backend` — lib `lattice_backend` + bin `lattice`.

## Package Manager

Cargo.

## Verified Commands

| Alias | Expands to |
|---|---|
| `just qg` | `cargo fmt --check` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo test --all` |
| `just test` | `cargo test --all` |
| `just fmt` | `cargo fmt` |
| `just fmt-check` | `cargo fmt --check` |
| `just lint` | `cargo clippy --all-targets --all-features -- -D warnings` |
| `just build` | `cargo build --all` |

**Quality gate:** `just qg` — zero output means pass. Run before every commit and sprint merge.

## Planned Crate Stack (added phase-by-phase; not all present yet)

Source: SPEC.md §5.1 (verified by reading the file this session).

| Concern | Crate |
|---|---|
| LSP scaffold | `tower-lsp` |
| Rust AST | `syn` |
| Generic AST | `tree-sitter` + grammars |
| File watching | `notify` |
| Runtime tracing | `tracing` + custom subscriber |
| Async runtime | `tokio` |
| WebSocket | `tokio-tungstenite` |
| Storage | `sqlx` (sqlite + postgres features) |
| Serialisation | `serde` / `serde_json` |
| Git metadata | `git2` |

## Dependency Audit Tool

`cargo audit` (requires `cargo-audit` installed). Run against `Cargo.lock`.

## House Conventions (hard contracts)

Source: SPEC.md §6, §11.1 and AGENT_PROTOCOL.md §6 (verified by reading both files this session).

**Doc comments on all public items:** Every `pub fn`, `pub struct`, `pub enum`, `pub mod`, and `pub trait` requires a `///` doc comment. Doc changes cascade up the hierarchy: modify a function → re-check and update the containing module doc and service doc. Hard contract because Lattice surfaces docs to non-technical stakeholders.

**Never panic on bad input:** Parser and recovery paths must never call `panic!`, `.unwrap()`, or `.expect()` outside `#[cfg(test)]`. On bad syntax: emit a partial tree, mark the node `error`, continue. Use `?` propagation or explicit `match`/`if let`.

**Idiomatic async Rust on tokio:** Use `tokio::spawn`, channels, `select!`. Never call blocking I/O on an async task — use `tokio::task::spawn_blocking` or async I/O primitives.

**Clippy `-D warnings` is the bar:** `just lint` must emit zero warnings before any change lands.

**TDD first:** Write a failing `#[test]` before implementing. Use table-driven tests (`let cases: Vec<(input, expected)> = vec![...]; for (input, want) in cases { ... }`) where natural.

**`// SAFETY:` on every `unsafe` block:** Explain the invariant being upheld.

**CLV wire protocol:** Sentinel `"#CLV1"`. Node IDs: `type:path:symbol`. The sentinel is the sole stable testable seam in the scaffold.

## Anti-patterns

- `.unwrap()` / `.expect()` outside `#[cfg(test)]`
- Blocking I/O inside `async fn`
- `panic!` in parser/recovery code paths
- Missing `///` doc comments on public items
- Stale module-level docs after a function's behaviour changes
- Missing `// SAFETY:` on `unsafe` blocks
- Speculative abstractions or features beyond the current phase
