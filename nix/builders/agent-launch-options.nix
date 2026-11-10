# launchOptions of the agent builders: model and effort are reserved and take
# a list of names, each passed to the agent as given by `flags.NAME`; every
# other option is mkGoblin's full form.
{ lib }:
builder: flags: launchOptions:
let
  fail = message: throw "${builder}: ${message}";
  reserved = lib.concatStringsSep " and " (builtins.attrNames flags);
  expand =
    name: value:
    if flags ? ${name} then
      if builtins.isList value && value != [ ] && builtins.all builtins.isString value then
        { choices = lib.genAttrs value (choice: { args = flags.${name} choice; }); }
      else
        fail "launchOptions.${name} must be a nonempty list of names, passed to the agent as given"
    else if builtins.isList value then
      fail "launchOptions.${name} needs the full form { description ? null; choices; }; only ${reserved} take a list"
    else
      value;
in
if builtins.isAttrs launchOptions then
  lib.mapAttrs expand launchOptions
else
  fail "launchOptions must be an attribute set"
