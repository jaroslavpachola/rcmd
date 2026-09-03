default: run

run:
    cargo run -p rcmd-tui

gui:
    cargo run -p rcmd-egui

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

e2e:
    cargo build -p rcmd-tui
    python3 tests/e2e/run.py
    RCMD_E2E_SUBSHELL=1 python3 tests/e2e/run.py

check: test lint e2e

# the window's desktop entry, Exec pinned to the installed binary:
# a GUI session rarely has ~/.cargo/bin on PATH
install-desktop:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$(command -v rcmd-egui || true)"
    [ -n "$bin" ] || bin="$HOME/.cargo/bin/rcmd-egui"
    [ -x "$bin" ] || { echo "no rcmd-egui binary: cargo install rcmd-egui" >&2; exit 1; }
    apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    mkdir -p "$apps"
    sed "s|^Exec=rcmd-egui$|Exec=$bin|; s|^TryExec=rcmd-egui$|TryExec=$bin|" \
        crates/rcmd-egui/dist/rcmd-egui.desktop > "$apps/rcmd-egui.desktop"
    command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
    echo "installed $apps/rcmd-egui.desktop -> $bin"
