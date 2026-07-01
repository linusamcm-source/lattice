# Lattice Phase 10 — Packaging

Phase 10 makes Lattice **downloadable and runnable as a single binary**: the `lattice` binary
serves the SvelteKit UI itself (so `lattice <dir>` + a browser shows a live graph with no separate
frontend process), a GitHub Actions workflow builds cross-platform binaries and publishes them to
GitHub Releases on a tag, and `cargo install` produces the same self-contained binary. This is the
`BUILD_PLAN.md` §"Phase 10" deliverable and the final phase.

**System-level "done" (BUILD_PLAN.md §Phase 10 accept):** a fresh machine on Windows / macOS / Linux
can download-and-run, point at a repo, and see a live graph. Today the binary is WS-only and the UI
runs as a separate Vite process — Phase 10 closes that gap.

## Grounding — what already exists (verified by recon this session)

- **The binary is `lattice`** — `crates/backend/Cargo.toml:10-11` (`[[bin]] name = "lattice"`,
  package `lattice-backend` v0.1.0). `main.rs` reads a repo dir arg (default `.`) + `LATTICE_ADDR`
  (default `127.0.0.1:7000`), calls `lattice_backend::run(root, addr)`, prints the bound addr, runs
  until Ctrl-C. So `cargo install --path crates/backend` already yields a runnable `lattice` — but it
  serves **only** WebSocket.
- **The server is raw `tokio-tungstenite`, WS-only.** `ws.rs:92` binds a `TcpListener`; `ws.rs:137`
  does `tokio_tungstenite::accept_async(stream)` on **every** accepted connection — there is **no**
  HTTP routing and **no** static-file serving (triple-verified: no `axum`/`hyper`/`Router`/`ServeDir`
  in `ws.rs` or `Cargo.toml`; only `tokio-tungstenite = "0.24"`). A plain browser GET currently fails
  the WS handshake.
- **The frontend builds to static files** — `frontend/svelte.config.js:1,8` uses
  `@sveltejs/adapter-static`; `frontend/README`/`CLAUDE.md` confirm a static SPA. There is **no**
  `frontend/build` committed and **nothing** embeds or serves it from Rust.
- **The frontend WS URL is hardcoded** — `frontend/src/routes/+page.svelte` `WS_URL =
  'ws://127.0.0.1:7000'` (verified in Phase 9). When the binary serves the UI on its own port, the
  client must derive the WS URL from `window.location` instead of the hardcode, so it works on any
  host/port.
- **No CI exists** — there is **no** `.github/workflows/` directory. No release automation, no
  `cargo install` docs (`README.md:108` only documents `cargo run -p lattice-backend -- <dir>`).
- **Only a `build` justfile target** exists (`justfile:19`); no `release`/`dist`/`package` target.
- **sqlx** uses `default-features = false` + `["runtime-tokio", "sqlite", "postgres"]`
  (`crates/backend/Cargo.toml:43-47`). The `sqlite` feature pulls `libsqlite3-sys` (bundled by
  default) so a static SQLite build is cross-platform; Postgres uses the pure-Rust `sqlx` driver — no
  system libpq needed. Confirm no other C system-lib deps break a clean cross-compile.

## Design decisions (deliberate, grounded)

1. **Single-port HTTP + WS via NON-DESTRUCTIVE discrimination — no framework swap.** Because the
   server is raw `tokio-tungstenite` on one `TcpListener` and `accept_async(stream)` (`ws.rs:137`)
   **consumes** the stream to read the client's upgrade request, P10-1 must inspect the request head
   **without consuming it**: `tokio::net::TcpStream::peek()` into a bounded buffer, then — if the head
   contains `Upgrade: websocket` — pass the **intact** stream to the existing `accept_async` path
   unchanged (so every Phase-0…9 WS test still passes); otherwise serve the embedded static asset for
   the request path, with `index.html` as the SPA fallback for extension-less routes. This avoids
   adding `axum`/`hyper` and keeps the one-port "download-and-run" model. (Adversarial review H-2.)
2. **The embed folder must EXIST at compile time; assets are built into it.** `rust-embed`'s derive
   **panics at compile time** (even in debug) if the `#[folder]` path is absent (adversarial review
   H-1), so a committed `frontend/build/.gitkeep` guarantees the dir exists for a fresh `cargo build`/
   `just qg`/worktree; `build.rs` also creates it defensively. `.gitignore` ignores `frontend/build/*`
   but **un-ignores** `.gitkeep`. Assets come from `npm --prefix frontend run build`. Pin the folder
   with `$CARGO_MANIFEST_DIR` via rust-embed's `interpolate-folder-path` feature so it resolves to the
   crate manifest dir (not the binary's runtime cwd — adversarial review M-1) in both debug and
   release. `--release` embeds the assets, so `cargo install`/release binaries are self-contained; a
   fresh checkout with only `.gitkeep` compiles + runs (WS works; UI 404s until built).
3. **Client derives the WS URL from `window.location`.** So the same embedded bundle works whether
   served by the binary (any host/port) or the Vite dev server; falls back to the dev default only
   when opened from Vite.
4. **CI validates by build + actionlint + review, not by a live release.** The release workflow can't
   be end-to-end run without pushing a tag; P10-2's DoD is a clean `actionlint`, pinned action SHAs,
   least-privilege `permissions`, and a green matrix build job — the actual publish is exercised on
   the first real tag.

---

## Story P10-1: Serve the embedded SvelteKit UI from the `lattice` binary (single port)

Make `lattice <dir>` serve the UI so a browser at the bound address shows a live graph with no Vite
process. Embed `frontend/build/` via `rust-embed` (`interpolate-folder-path` + `$CARGO_MANIFEST_DIR`,
Design Decision #2); in the connection handler (`ws.rs`) discriminate HTTP vs WS **non-destructively**
via `TcpStream::peek()` (Design Decision #1): a head with `Upgrade: websocket` hands the **intact**
stream to the existing `accept_async` path unchanged; any other GET serves the embedded asset for the
path (MIME by extension), falling back to `index.html` for extension-less SPA routes; a missing asset
with a real extension → 404. Commit `frontend/build/.gitkeep` (+ `.gitignore` un-ignore) and a
`build.rs` that ensures the dir exists so the derive never panics on a fresh checkout. Set
`adapter({ fallback: 'index.html' })` in `frontend/svelte.config.js` so `npm run build` emits
`frontend/build/index.html` (adversarial review M-2). Change the frontend `WS_URL` to derive from
`window.location` — scheme from `location.protocol` (`wss:`→`wss://`, else `ws://`) + `location.host`
(adversarial review L-1) — keeping the dev fallback.

### Depends On: none
### Touches: crates/backend/src/ws.rs, crates/backend/Cargo.toml, crates/backend/src/lib.rs, crates/backend/build.rs, frontend/src/routes/+page.svelte, frontend/src/lib/ws.ts, frontend/svelte.config.js, frontend/build/.gitkeep, .gitignore

### Acceptance Criteria
- With an embedded (or dev-dir) `index.html`, an HTTP `GET /` to the bound address returns `200` with
  `Content-Type: text/html` and the index body; `GET /assets/<x>.js` returns `200` with a JS MIME;
  an unknown SPA route `GET /some/deep/link` returns the `index.html` fallback (`200 text/html`); a
  request for a missing file with a real extension (`GET /nope.css`) returns `404`.
- A WebSocket client connecting to the **same** address+port still completes the handshake and
  receives the root snapshot (the existing WS behaviour is unchanged — the discrimination routes
  `Upgrade: websocket` to the untouched `accept_async` path). Every prior Phase-0…9 ws test still
  passes.
- The static-serving path is panic-free on a malformed/oversized HTTP request head (bounded read;
  never `unwrap`/`panic` on client bytes) — asserted by feeding a garbage/oversized request head.
- The frontend derives its WS URL from `window.location` — a unit test asserts `wss://host/` for an
  `https:` location and `ws://host/` for `http:`, plus the dev fallback when `location` is absent;
  `npm run check`/`npm run test` stay green. `frontend/svelte.config.js` sets
  `adapter({ fallback: 'index.html' })` and `npm run build` emits `frontend/build/index.html`.
- **A fresh checkout compiles** with only the committed `frontend/build/.gitkeep` present (no built
  frontend): `cargo build` + `just qg` succeed (the `rust-embed` folder exists, so the derive does
  not panic — adversarial review H-1); at runtime the WS path works and unbuilt UI assets return 404,
  so contributors are not blocked. `frontend/build/*` is git-ignored except `.gitkeep`.

### Definition of Done
- New `#[cfg(test)]` cases in `ws.rs` (HTTP GET index/asset/SPA-fallback/404, WS-still-works,
  malformed-head panic-freedom) and a `ws.ts`/frontend unit test for the WS-URL derivation; `just
  test` + `npm run test` green; new-code coverage ≥ 90% (backend) on the new serving code.
- `just qg` clean; `npm run check` zero errors, `npm run lint` clean.
- Doc-comment cascade: the new HTTP-serving path documented in `ws.rs` + the module/`lib.rs` doc
  updated to note the binary now serves the UI (`AGENT_PROTOCOL.md §6`).

## Story P10-2: Cross-platform release CI → GitHub Releases

Add `.github/workflows/release.yml` that, on a `v*` tag push, builds the `lattice` binary for
Linux / macOS / Windows with the frontend bundled and uploads the artifacts to a GitHub Release.
Build the frontend once (`npm ci` + `npm run build` in `frontend/`), then a matrix `cargo build
--release` per target, package each binary (+ the built UI, already embedded), and attach to the
Release. Security-harden per house/github-actions norms: pin every action to a full commit SHA,
set top-level `permissions: {}` and job-level least privilege (`contents: write` only on the publish
job), no long-lived secrets beyond the automatic `GITHUB_TOKEN`.

### Depends On: P10-1
### Touches: .github/workflows/release.yml

### Acceptance Criteria
- `.github/workflows/release.yml` triggers on `push: tags: ['v*']` and runs a **matrix** over
  `ubuntu-latest`/`macos-latest`/`windows-latest`. Because Actions matrix jobs do **not** share a
  filesystem and `rust-embed` (release) embeds relative to `Cargo.toml`, **each** matrix job runs
  `npm ci` + `npm run build` in `frontend/` **before** `cargo build --release` for the `lattice`
  binary — so `frontend/build/` exists in every runner's checkout (adversarial review M-3; no
  cross-job artifact passing of the build dir).
- A publish job creates/updates the GitHub Release for the tag and uploads each platform artifact
  (named with the target triple/OS), using only `GITHUB_TOKEN` with job-scoped `contents: write`.
- Every `uses:` action is pinned to a full 40-char commit SHA (no `@v4`/`@main`); the workflow sets a
  restrictive top-level `permissions` block.
- `actionlint` reports zero errors on the workflow (run in the sprint as the validation gate, since
  the release itself can't fire without a real tag).

### Definition of Done
- `.github/workflows/release.yml` present and `actionlint`-clean (install/run `actionlint` in the
  sprint; if unavailable, a documented manual YAML + pinning review by the github-actions reviewer).
- A short note in `README.md` (or `docs/`) that tagged releases publish binaries — cross-referenced
  by P10-3.
- No coverage gate applies (no Rust code); the gate is actionlint + the security review.

## Story P10-3: cargo-install path + build orchestration + download-and-run docs

Make `cargo install --path crates/backend` and a documented download-and-run flow both work and be
discoverable. Add package metadata (`description`, `license`, `repository`, `readme`) to
`crates/backend/Cargo.toml` so the crate is install/publish-clean; add justfile `release`/`dist`
targets that build the frontend then `cargo build --release` (so the embed is populated); and update
`README.md` with a "Install & run" section: `cargo install --path crates/backend` (or from a release
binary), then `lattice <dir>`, open the printed address. Ensure `frontend/build/` is git-ignored.

### Depends On: P10-1
### Touches: crates/backend/Cargo.toml, justfile, README.md, .gitignore

### Acceptance Criteria
- `crates/backend/Cargo.toml` `[package]` carries `description`, `license` (or `license-file`),
  `repository`, and `readme`; `cargo build --release` still succeeds and the metadata is valid
  (`cargo metadata`/`cargo publish --dry-run` style check passes locally, no packaging errors).
- A justfile `release` (and/or `dist`) target builds the frontend (`npm --prefix frontend run build`)
  then `cargo build --release -p lattice-backend`, producing a self-contained `lattice` that serves
  the UI (a smoke check: run the built binary, `GET /` returns the UI `200 text/html`).
- `README.md` has an "Install & run" section documenting `cargo install --path crates/backend` (and
  the release-binary path) + `lattice <dir>` + opening the printed address to see the live graph;
  the prior `cargo run` note remains accurate.
- `frontend/build/` is in `.gitignore` (not committed); a clean checkout + the `release` target
  reproduces a working binary.

### Definition of Done
- `cargo build --release` + the justfile `release` target both succeed; the smoke check (built binary
  serves `GET / → 200 text/html`) passes; `just qg` unaffected (no source logic change beyond
  metadata).
- README updated + accurate; `.gitignore` covers `frontend/build/`.
- No new-code coverage gate applies (metadata/docs/justfile); validated by the successful release
  build + smoke check + review.

---

## Dependency graph

```
P10-1  Serve embedded UI from the binary (ws.rs, Cargo.toml, frontend WS URL, build.rs) . Depends: none
  ├─ P10-2  Cross-platform release CI → GitHub Releases (.github/workflows/release.yml) . Depends: P10-1
  └─ P10-3  cargo-install path + build orchestration + docs (Cargo.toml, justfile, README) Depends: P10-1
```

- **Acyclic.** P10-1 (the binary actually serving the UI) gates both the CI that ships it (P10-2) and
  the install/docs that describe it (P10-3). P10-2 and P10-3 are independent of each other (different
  files) and can run in parallel after P10-1.
- **Stack:** P10-1 is cross-stack (Rust server + a small frontend WS-URL change); P10-2 is CI YAML;
  P10-3 is Cargo/justfile/docs. Coverage gate applies only to P10-1's new Rust serving code; P10-2/3
  are validated by actionlint / build + smoke check + review.
- **Phase-10 accept (BUILD_PLAN) is met when:** P10-1 makes `lattice <dir>` serve a live graph in a
  browser from one binary, P10-3 makes `cargo install` + download-and-run documented and working, and
  P10-2 publishes cross-platform release binaries on tag.
