{ pkgs }:
let
  inherit (pkgs) lib;
  validate = import ../internal/validate.nix { inherit lib; };
in
{
  persistent ? true,
  namespaces ? { },
  storage ? { },
  docker ? { },
  defaults ? { },
}:
let
  pid = namespaces.pid.enable or true;
  # A storage path is the default mount location for every member; a goblin's
  # own scopeStorage entry (or the scope's defaults) can override it.
  paths = lib.filterAttrs (_: path: path != null) (
    builtins.mapAttrs (_: options: options.path or null) storage
  );
in
assert lib.assertMsg (builtins.isBool persistent) "mkScope: persistent must be a boolean";
assert lib.assertMsg (
  builtins.isAttrs namespaces
  && builtins.all (name: name == "pid") (builtins.attrNames namespaces)
  && builtins.isAttrs (namespaces.pid or { })
  && builtins.all (name: name == "enable") (builtins.attrNames (namespaces.pid or { }))
  && builtins.isBool pid
) "mkScope: namespaces accepts only pid.enable (boolean); the network namespace is always shared";
assert lib.assertMsg (
  builtins.isAttrs storage
  && builtins.all (
    name:
    validate.validName name
    && builtins.isAttrs storage.${name}
    && builtins.all (option: option == "path") (builtins.attrNames storage.${name})
  ) (builtins.attrNames storage)
  && validate.storageMounts paths
) "mkScope: storage maps simple names to { path = \"$HOME/.cache/tool\"; } (path is optional)";
assert lib.assertMsg (
  builtins.isAttrs docker
  && builtins.all (name: name == "enable") (builtins.attrNames docker)
  && builtins.isBool (docker.enable or false)
) "mkScope: docker accepts only enable (boolean)";
assert validate.scopeDefaults defaults;
{
  _type = "goblinsScope";
  inherit persistent pid;
  defaults = defaults // {
    scopeStorage = paths // (defaults.scopeStorage or { });
  };
  storage = builtins.attrNames storage;
  docker = docker.enable or false;
}
