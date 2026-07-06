default: run

run:
    cargo run -p rcmd-tui

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

e2e:
    cargo build
    python3 tests/e2e/run.py
    RCMD_E2E_SUBSHELL=1 python3 tests/e2e/run.py

check: test lint e2e
