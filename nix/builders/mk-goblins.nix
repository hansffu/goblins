{ pkgs }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
in
{
  goblins,
  scopes ? { },
  docker ? null,
}:
let
  fail = message: throw "mkGoblins: ${message}";
  names =
    goblin: lib.unique (lib.optional (goblin.scope != null) goblin.scope ++ goblin.allowed_scopes);
  # One complete configuration per scope: scope defaults are merged in Nix.
  variant =
    name: value: scope:
    let
      definition = scopes.${scope} or (fail "'${name}' references undeclared scope '${scope}'");
      goblin = (value.withScopeDefaults definition.defaults).goblin;
      missing = lib.subtractLists definition.storage (builtins.attrNames goblin.scope_storage);
    in
    if missing != [ ] then
      fail "'${name}' mounts storage not declared by scope '${scope}': ${lib.concatStringsSep ", " missing}"
    else
      builtins.removeAttrs goblin [
        "scope"
        "allowed_scopes"
      ];
  configurations = lib.mapAttrs (
    name: value:
    if !(builtins.isAttrs value && value ? goblin && value ? withScopeDefaults) then
      fail "'${name}' must be built with mkGoblin"
    else if value.goblin.scope == null && value.goblin.scope_storage != { } then
      fail "'${name}' sets scopeStorage without a default scope"
    else if
      value.goblin.docker.enabled
      && (
        value.goblin.scope == null
        || !builtins.all (scope: scopes.${scope}.docker or true) (names value.goblin)
      )
    then
      fail "'${name}' sets docker.enable, so it needs a default scope and every scope it may join must enable Docker"
    else
      value.goblin
      // {
        scopes = lib.genAttrs (names value.goblin) (variant name value);
      }
  ) goblins;
  # Members share one network namespace, so they must agree on network policy.
  networks = lib.mapAttrs (
    scope: _:
    lib.unique (
      lib.concatMap (
        configuration: lib.optional (configuration.scopes ? ${scope}) configuration.network
      ) (builtins.attrValues configurations)
    )
  ) scopes;
in
assert validate.goblins goblins;
assert
  docker == null
  || fail "docker.scopes was replaced by scopes = { NAME = mkScope { docker.enable = true; }; }";
assert lib.assertMsg (
  builtins.isAttrs scopes
  && builtins.all (
    name: validate.scopes [ name ] && (scopes.${name}._type or null) == "goblinsScope"
  ) (builtins.attrNames scopes)
) "mkGoblins: scopes must map simple scope names to mkScope results";
assert lib.assertMsg (builtins.all (n: builtins.length n <= 1) (
  builtins.attrValues networks
)) "mkGoblins: members of a scope must have the same allowedDomains network policy";
import ../packages/goblins.nix {
  inherit pkgs goblins configurations;
  scopes = lib.mapAttrs (_: scope: {
    inherit (scope)
      persistent
      pid
      storage
      docker
      ;
  }) scopes;
}
