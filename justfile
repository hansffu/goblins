# Open the user's Emacs with this checkout's elisp on the load-path and its
# demo goblins as the CLI. The dev-goblins package is not on the dev shell's
# PATH, so `goblins-executable' points at it.
[positional-arguments]
emacs *args:
    #!/usr/bin/env bash
    set -euo pipefail
    goblins=$(nix build --no-link --print-out-paths .#dev-goblins)
    setup="(progn
      (add-to-list 'load-path \"{{justfile_directory()}}/emacs\")
      (setq goblins-executable \"$goblins/bin/goblins\")
      (let ((state (getenv \"GOBLINS_STATE_DIR\")))
        (when state (setq goblins-state-directory state)))
      (require 'goblins-workspace))"
    exec nix develop -c emacs --eval "$setup" "$@"

# Emacs exiting stops that server and removes its state directory.
# `just emacs` with its own server beside yours, in a random GOBLINS_STATE_DIR
[positional-arguments]
emacs-daemon *args:
    #!/usr/bin/env bash
    set -euo pipefail
    base=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/goblins-dev.XXXXXX")
    export GOBLINS_STATE_DIR=$base/state
    goblins=$(nix build --no-link --print-out-paths .#dev-goblins)/bin/goblins
    # Keep the directory when the server did not stop: it is how to reach it.
    cleanup() {
      if "$goblins" --state-dir "$GOBLINS_STATE_DIR" server stop >/dev/null; then
        rm -rf "$base"
      else
        echo "Kept $GOBLINS_STATE_DIR; stop it with: $goblins --state-dir $GOBLINS_STATE_DIR server stop" >&2
        exit 1
      fi
    }
    trap cleanup EXIT
    echo "Goblins state directory: $GOBLINS_STATE_DIR (goblins --state-dir \"\$GOBLINS_STATE_DIR\" …)"
    {{just_executable()}} --justfile {{justfile()}} emacs "$@"

# Enter `nix develop` once, then run these commands directly.
# Rust/Python/Emacs suites are serial; Python defaults to two workers (-j 4 opts in).
test: test-rust test-python test-emacs

test-rust:
    cargo test --workspace --locked -j 2 -- --test-threads=2

[positional-arguments]
test-python *args:
    python3 tests/run.py "$@"

# List named tasks, preview selections, or run a union of features.
test-features:
    python3 tests/tasks.py --list

[positional-arguments]
test-plan *args:
    python3 tests/tasks.py --plan "$@"

[positional-arguments]
test-feature *args:
    python3 tests/tasks.py "$@"

test-feature-check:
    python3 tests/tasks.py --check

# Required on the final candidate before merging, not before requesting review.
[positional-arguments]
test-merge *args:
    python3 tests/tasks.py full "$@"

# Release retained test fixtures after all tests/standalone test daemons stop.
test-cache-clean:
    python3 tests/run.py --clean-cache

# Nix removes dangling indirect-root metadata while finding roots. This does
# not collect store outputs or remove live roots; applies to all stale links.
test-roots-clean:
    nix-store --gc --print-roots --quiet > /dev/null

test-emacs:
    emacs --batch -Q -l tests/emacs-init.el -l goblins-tests -f ert-run-tests-batch-and-exit

# Full packaging/Nix validation stays available separately from the edit loop.
test-nix:
    nix flake check --max-jobs 1 --cores 2

[positional-arguments]
test-packaged *args:
    python3 tests/run.py --packaged "$@"
