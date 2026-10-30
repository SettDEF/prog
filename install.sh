#!/usr/bin/env bash
# Builds prog (any command's progress as a floating card on the desktop) and installs it to ~/.local/bin.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --quiet ${JOBS:+-j "$JOBS"}
mkdir -p "$HOME/.local/bin"
install -m755 "${CARGO_TARGET_DIR:-target}/release/prog" "$HOME/.local/bin/prog"
echo "prog installed: prog -- COMMAND"
