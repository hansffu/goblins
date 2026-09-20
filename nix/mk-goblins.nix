{ pkgs }:
let
  validate = import ./internal/validate.nix { inherit (pkgs) lib; };
in
{ goblins }:
assert validate.goblins goblins;
import ./packages/goblins.nix {
  inherit pkgs goblins;
  configurations = pkgs.lib.mapAttrs (
    name: value:
    if builtins.isAttrs value && value ? goblin then
      value.goblin
    else
      throw "mkGoblins: '${name}' must be built with mkGoblin"
  ) goblins;
}
