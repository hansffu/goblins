{ pkgs, sandbox }:
let
  inherit (pkgs) lib;
  fail = message: throw "mkGoblin: ${message}";
  validName = name: builtins.match "[A-Za-z_][A-Za-z0-9_-]*" name != null;
  inner = import ./packages/internal-goblins.nix { inherit pkgs; };

  mkGoblin =
    {
      pkg,
      binName,
      outName ? "goblin-${binName}",
      allowedPackages ? sandbox.commonTools,
      args ? [ ],
      env ? { },
      allowNix ? false,
      allowUnixSockets ? true,
      allowedDomains ? [ ],
      allowedHostPorts ? [ ],
      publishedPorts ? [ ],
      rwDirs ? [ ],
      rwFiles ? [ ],
      roDirs ? [ ],
      roFiles ? [ ],
    }:
    if !validName binName || !validName outName then
      fail "binName and outName must be simple executable names"
    else if allowNix != false then
      fail "host Nix access is prohibited; request packages through goblins"
    else if allowUnixSockets != true then
      fail "allowUnixSockets must be true for the session request socket"
    else if allowedDomains != [ ] || allowedHostPorts != [ ] || publishedPorts != [ ] then
      fail "network grants are not supported by the live-mount launcher yet"
    else if rwDirs != [ ] || rwFiles != [ ] || roDirs != [ ] || roFiles != [ ] then
      fail "host binds are not supported yet; use goblins serve --workspace for a disposable snapshot"
    else if !builtins.isList args || !builtins.all builtins.isString args then
      fail "args must be a list of strings"
    else if
      !builtins.isAttrs env
      || !builtins.all (
        name: builtins.match "[A-Za-z_][A-Za-z0-9_]*" name != null && builtins.isString env.${name}
      ) (builtins.attrNames env)
    then
      fail "env must map environment variable names to literal strings"
    else if
      lib.intersectLists (builtins.attrNames env) [
        "PATH"
        "HOME"
        "SHELL"
        "USER"
        "LOGNAME"
        "PYTHONNOUSERSITE"
      ] != [ ]
    then
      fail "env cannot override PATH, HOME, SHELL, USER, LOGNAME or PYTHONNOUSERSITE"
    else
      let
        wrapped = sandbox.mkSandbox {
          inherit
            pkg
            binName
            outName
            env
            allowNix
            allowUnixSockets
            allowedDomains
            allowedHostPorts
            publishedPorts
            rwDirs
            rwFiles
            roDirs
            roFiles
            ;
          allowedPackages = allowedPackages ++ [ inner ];
        };
      in
      wrapped
      // {
        goblin = {
          build_spec = wrapped.buildSpec;
          inherit args env;
          client_package = inner;
        };
      };

  mkGoblins =
    { goblins }:
    if !builtins.isAttrs goblins || goblins == { } then
      throw "mkGoblins: goblins must be a nonempty attrset of mkGoblin results"
    else if !builtins.all validName (builtins.attrNames goblins) then
      throw "mkGoblins: goblin names must be simple identifiers"
    else
      let
        configurations = lib.mapAttrs (
          name: value:
          if builtins.isAttrs value && value ? goblin then
            value.goblin
          else
            throw "mkGoblins: '${name}' must be built with mkGoblin"
        ) goblins;
      in
      import ./packages/goblins.nix { inherit pkgs goblins configurations; };
in
{
  inherit mkGoblin mkGoblins;
  inherit (sandbox) commonTools;
  goblins.shell = import ./goblins/shell.nix { inherit pkgs mkGoblin; };
}
