{ pkgs }:
pkgs.writeScriptBin "goblins" ''
  #!${pkgs.python3}/bin/python3
  ${builtins.readFile ../../request.py}
''
