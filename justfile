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
# Keep suites serial: they exercise namespaces, Docker engines and process death.
test: test-rust test-python test-emacs

test-rust:
    cargo test --workspace --locked -j 2 -- --test-threads=2

[positional-arguments]
test-python *args:
    python3 tests/run.py "$@"

test-emacs:
    emacs --batch -Q -l tests/emacs-init.el -l goblins-tests -f ert-run-tests-batch-and-exit

# Full packaging/Nix validation stays available separately from the edit loop.
test-nix:
    nix flake check --max-jobs 1 --cores 2

[positional-arguments]
test-packaged *args:
    python3 tests/run.py --packaged "$@"
