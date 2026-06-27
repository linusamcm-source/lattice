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