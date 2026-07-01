//! Build script for `lattice-backend`.
//!
//! The `ws` module embeds the built SvelteKit UI from `frontend/build/` via
//! `rust-embed` (P10-1). The `rust-embed` derive macro reads that folder at compile
//! time and fails to compile if it is absent, so this script defensively creates it
//! before the crate is compiled — a fresh checkout whose generated `frontend/build/`
//! has not been produced yet (no `npm run build`) still builds instead of erroring.
//! A committed `frontend/build/.gitkeep` also keeps the folder present in git; this
//! script covers checkouts/CI where the directory may be pruned.

use std::path::Path;

/// Ensures the `frontend/build/` embed folder exists before compilation.
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let build_dir = Path::new(&manifest_dir).join("../../frontend/build");
    // Best-effort: a pre-existing folder or a permission error must not fail the
    // build here — the `rust-embed` macro surfaces a real, actionable error if the
    // folder is genuinely unusable.
    let _ = std::fs::create_dir_all(&build_dir);
    // Re-run when the embedded bundle changes so fresh assets are picked up.
    println!("cargo:rerun-if-changed=../../frontend/build");
}
