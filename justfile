opus:
    @claude --dangerously-skip-permissions "/caveman"

# Quality gate: run before every commit / sprint merge.
qg: fmt-check lint test

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all

build:
    cargo build --all

# Run backend (127.0.0.1:7000, override LATTICE_ADDR) + frontend dev together.
# Open http://localhost:5173 once both are up; Ctrl-C stops both.
run dir=".":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run -p lattice-backend -- "{{dir}}" &
    backend=$!
    trap 'kill $backend 2>/dev/null || true' EXIT
    npm --prefix frontend run dev

# Run only the backend (LATTICE_ADDR overrides the 127.0.0.1:7000 default).
backend dir=".":
    cargo run -p lattice-backend -- "{{dir}}"

# Run only the frontend dev server (expects the backend on :7000).
dev:
    npm --prefix frontend run dev