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
  && builtins.all (name: validate.validName name && storage.${name} == { }) (
    builtins.attrNames storage
  )
) "mkScope: storage maps simple names to { }";
assert lib.assertMsg (
  builtins.isAttrs docker
  && builtins.all (name: name == "enable") (builtins.attrNames docker)
  && builtins.isBool (docker.enable or false)
) "mkScope: docker accepts only enable (boolean)";
assert validate.scopeDefaults defaults;
{
  _type = "goblinsScope";
  inherit persistent pid defaults;
  storage = builtins.attrNames storage;
  docker = docker.enable or false;
}
