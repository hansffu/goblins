{ pkgs }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
in
{
  goblins,
  scopes ? { },
  docker ? null,
  # editor.emacs.enable: sandboxes open files in the host's Emacs over TRAMP
  # (EDITOR and VISUAL), and get the tools the /goblins: TRAMP method needs.
  editor ? { },
}:
let
  fail = message: throw "mkGoblins: ${message}";
  emacs = {
    enable = false;
    command = "emacsclient";
  }
  // (editor.emacs or { });
  # Defaults for every goblin; a goblin's own env still wins.
  editorDefaults = {
    allowedPackages = with pkgs; [
      coreutils
      gnused
      gnugrep
      findutils
      gawk
    ];
    env = {
      EDITOR = "/run/goblins/bin/goblins-editor";
      VISUAL = "/run/goblins/bin/goblins-editor";
    };
  };
  withEditor = value: if emacs.enable then value.withScopeDefaults editorDefaults else value;
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
        "allowed_children"
      ];
  # Omitted: only the configuration itself. The daemon applies the same bounds.
  allowedChildren =
    name: goblin:
    let
      children = if goblin.allowed_children == null then [ name ] else goblin.allowed_children;
      undeclared = builtins.filter (child: !(goblins ? ${child})) children;
    in
    if builtins.length children > 64 then
      fail "'${name}' allowedChildren lists more than 64 goblins"
    else if builtins.any (child: builtins.stringLength child > 64) children then
      fail "'${name}' allowedChildren names must be at most 64 characters"
    else if undeclared != [ ] then
      fail "'${name}' allowedChildren references undeclared goblin '${builtins.head undeclared}'"
    else
      children;
  configurations = lib.mapAttrs (
    name: original:
    let
      value = withEditor original;
    in
    if !(builtins.isAttrs original && original ? goblin && original ? withScopeDefaults) then
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
        allowed_children = allowedChildren name value.goblin;
      }
  ) goblins;
  # The values of `field` among the configurations that may join each scope.
  perScope =
    field:
    lib.mapAttrs (
      scope: _:
      lib.unique (
        lib.concatMap (
          configuration: lib.optional (configuration.scopes ? ${scope}) configuration.${field}
        ) (builtins.attrValues configurations)
      )
    ) scopes;
  # Members share one network namespace, so they must agree on network policy.
  networks = perScope "network";
  # A shared Docker engine's store view comes from its members, so they must
  # agree on allowNix in a scope with Docker.
  nixAccess = lib.filterAttrs (scope: _: scopes.${scope}.docker) (perScope "nix");
in
assert validate.goblins goblins;
assert lib.assertMsg (
  builtins.isAttrs editor
  && builtins.all (name: name == "emacs") (builtins.attrNames editor)
  && builtins.isAttrs (editor.emacs or { })
  && builtins.all (
    name:
    builtins.elem name [
      "enable"
      "command"
    ]
  ) (builtins.attrNames (editor.emacs or { }))
  && builtins.isBool emacs.enable
  && builtins.isString emacs.command
  && emacs.command != ""
) "mkGoblins: editor accepts only emacs = { enable = BOOL; command = \"emacsclient\"; }";
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
assert lib.assertMsg (builtins.all (n: builtins.length n <= 1) (
  builtins.attrValues nixAccess
)) "mkGoblins: members of a scope with Docker must have the same allowNix";
import ../packages/goblins.nix {
  inherit pkgs goblins configurations;
  editor =
    if emacs.enable then
      {
        kind = "emacs";
        inherit (emacs) command;
      }
    else
      null;
  scopes = lib.mapAttrs (_: scope: {
    inherit (scope)
      persistent
      pid
      storage
      docker
      ;
  }) scopes;
}
