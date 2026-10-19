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
      (require 'goblins-workspace))"
    exec nix develop -c emacs --eval "$setup" "$@"

# Enter `nix develop` once, then run these commands directly.
# Rust/Python/Emacs suites are serial; Python defaults to two workers (-j 4 opts in).
test: test-rust test-python test-emacs

test-rust:
    cargo test --workspace --locked -j 2 -- --test-threads=2

[positional-arguments]
test-python *args:
    python3 tests/run.py "$@"

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
