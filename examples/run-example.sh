#!/usr/bin/env bash
# Runs runandlog against a throwaway copy of exam.md. Try things out with this.
#
#   examples/run-example.sh              # open the TUI
#   examples/run-example.sh --run-all    # run every cell
#   examples/run-example.sh --gui        # open the desktop app
#
# Any arguments are passed straight through. A fresh copy is made on every call,
# so no number of trials shows up in `git status`.
#
# The binary is built from the current tree unless RUNANDLOG names one to use:
#
#   RUNANDLOG=./target/release/runandlog examples/run-example.sh --run-all

set -euo pipefail

examples_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
document=$("$examples_dir/create-test-copy.sh")

if [[ -n ${RUNANDLOG:-} ]]; then
    exec "$RUNANDLOG" "$document" "$@"
fi
# --quiet so that cargo's progress does not mix into the TUI's first frame.
exec cargo run --quiet --manifest-path "$examples_dir/../Cargo.toml" \
    -p runandlog -- "$document" "$@"
