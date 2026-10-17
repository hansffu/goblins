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
