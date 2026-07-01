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

1. **Single-port HTTP + WS via minimal request discrimination — no framework swap.** Because the
   server is raw `tokio-tungstenite` on one `TcpListener`, P10-1 keeps that model: on each accepted
   connection, read the HTTP request head; if it carries `Upgrade: websocket` route to the existing
   `accept_async` WS path unchanged, otherwise serve the embedded static asset for the request path
   (with `index.html` as the SPA fallback for unknown paths). This avoids adding `axum`/`hyper` and
   keeps the one-port "download-and-run" model. The frontend is embedded at compile time via
   `rust-embed` so the binary is self-contained.
2. **Frontend build feeds the embed via a documented build step, not a git-committed `build/`.** The
   embedded assets come from `frontend/build/`, produced by `npm --prefix frontend run build`. A
   `build.rs` (or the justfile/CI) runs the frontend build before `cargo build` so the embed picks up
   fresh assets; `frontend/build/` stays git-ignored. `rust-embed`'s `debug-embed`-off default reads
   from disk in dev and embeds in `--release`, so `cargo install`/release builds are self-contained
   while dev stays fast.
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
process. Embed `frontend/build/` via `rust-embed`; in the connection handler (`ws.rs`) discriminate
HTTP vs WS: a request with `Upgrade: websocket` takes the existing `accept_async` path unchanged; any
other GET serves the embedded asset for the path (MIME by extension), falling back to `index.html`
for SPA routes; unknown asset under a real extension → 404. Change the frontend `WS_URL` to derive
from `window.location` (`ws://<host>:<port>/` from `location.host`), keeping a dev fallback. Add a
`build.rs` (or justfile step) note so the embed sees a built frontend.

### Depends On: none
### Touches: crates/backend/src/ws.rs, crates/backend/Cargo.toml, crates/backend/src/lib.rs, frontend/src/routes/+page.svelte, frontend/src/lib/ws.ts, build.rs

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
- The frontend derives its WS URL from `window.location` (a unit test asserts the derivation yields
  `ws://<host>/` for a given `location`, and the dev fallback when `location` is absent); `npm run
  check`/`npm run test` stay green.
- `just qg` is green (backend), and the embed does not break a normal `cargo build` when
  `frontend/build/` is absent (dev reads from disk / a clear build-time message), so contributors
  aren't blocked.

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
- `.github/workflows/release.yml` triggers on `push: tags: ['v*']`, has a **frontend-build** step
  (`npm ci` + `npm run build` in `frontend/`) preceding the Rust build, and a **matrix** over
  `ubuntu-latest`/`macos-latest`/`windows-latest` running `cargo build --release` for the `lattice`
  binary.
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
