{ lib }:
let
  fail = message: throw "mkGoblin: ${message}";
  validName = name: builtins.match "[A-Za-z_][A-Za-z0-9_-]*" name != null;
in
{
  goblin =
    options:
    let
      inherit (options)
        binName
        outName
        integration
        allowNix
        allowUnixSockets
        allowedDomains
        allowedHostPorts
        publishedPorts
        rwDirs
        rwFiles
        roDirs
        roFiles
        args
        injectedFiles
        env
        ;
    in
    if !validName binName || !validName outName then
      fail "binName and outName must be simple executable names"
    else if integration != null && integration != "codex" then
      fail "unsupported integration driver"
    else if allowNix != false then
      fail "host Nix access is prohibited; request packages through goblins"
    else if allowUnixSockets != true then
      fail "allowUnixSockets must be true for the session request socket"
    else if
      (allowedDomains != null && allowedDomains != [ ])
      || allowedHostPorts != [ ]
      || publishedPorts != [ ]
    then
      fail "allowedDomains must be null (open) or [] (offline); domain filters and port mappings are not supported"
    else if
      !builtins.all (paths: builtins.isList paths && builtins.all builtins.isString paths) [
        rwDirs
        rwFiles
        roDirs
        roFiles
      ]
    then
      fail "rwDirs, rwFiles, roDirs and roFiles must be lists of path strings"
    else if !builtins.isList args || !builtins.all builtins.isString args then
      fail "args must be a list of strings"
    else if
      !builtins.isAttrs injectedFiles
      || !builtins.all (
        name:
        builtins.match "[A-Za-z0-9_-][A-Za-z0-9_.-]*(/[A-Za-z0-9_-][A-Za-z0-9_.-]*)*" name != null
        && (builtins.isString injectedFiles.${name} || lib.isDerivation injectedFiles.${name})
      ) (builtins.attrNames injectedFiles)
    then
      fail "injectedFiles must map relative /etc file names to text or file derivations"
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
      true;

  goblins =
    goblins:
    if !builtins.isAttrs goblins || goblins == { } then
      throw "mkGoblins: goblins must be a nonempty attrset of mkGoblin results"
    else if !builtins.all validName (builtins.attrNames goblins) then
      throw "mkGoblins: goblin names must be simple identifiers"
    else
      true;
}
