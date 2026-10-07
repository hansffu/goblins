# Open the user's Emacs from the dev shell, with this checkout's elisp on the
# load-path.  The dev shell's `goblins' comes first on PATH, so Emacs runs the
# packaged-from-source CLI instead of the installed one.
emacs *args:
    nix develop -c emacs --eval "(progn (add-to-list 'load-path \"{{justfile_directory()}}/emacs\") (require 'goblins-workspace))" {{args}}
