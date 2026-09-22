{ pkgs }:
let
  validate = import ../internal/validate.nix { inherit (pkgs) lib; };
in
{
  goblins,
  docker ? { },
}:
let
  scopes = docker.scopes or [ ];
  configurations = pkgs.lib.mapAttrs (
    name: value:
    if builtins.isAttrs value && value ? goblin then
      let
        d = value.goblin.docker;
      in
      assert pkgs.lib.assertMsg (builtins.all (scope: builtins.elem scope scopes) (
        d.allowed_scopes ++ pkgs.lib.optional (d.default_scope != null) d.default_scope
      )) "mkGoblins: '${name}' references an undeclared Docker scope";
      value.goblin
    else
      throw "mkGoblins: '${name}' must be built with mkGoblin"
  ) goblins;
in
assert validate.goblins goblins;
assert pkgs.lib.assertMsg (
  builtins.isAttrs docker
  && builtins.all (n: n == "scopes") (builtins.attrNames docker)
  && validate.scopes scopes
) "mkGoblins: docker.scopes must be a list of unique simple scope names";
import ../packages/goblins.nix {
  inherit pkgs goblins configurations;
  dockerScopes = scopes;
}
