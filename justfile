# Open the user's Emacs with this checkout's elisp on the load-path and its
# demo goblins as the CLI: `goblins-executable' is the dev-goblins package,
# which is not on the dev shell's PATH.
emacs *args:
    goblins=$(nix build --no-link --print-out-paths .#dev-goblins) && nix develop -c emacs --eval "(progn (add-to-list 'load-path \"{{justfile_directory()}}/emacs\") (setq goblins-executable \"$goblins/bin/goblins\") (require 'goblins-workspace))" {{args}}
