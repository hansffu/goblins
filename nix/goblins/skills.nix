# The Goblins skills a sandbox gets, by feature. A goblin with Nix runs
# ordinary nix commands, so it gets goblins-nix instead of the package and dev
# shell skills, whose workflows would contradict it.
{ lib }:
{
  nix ? false,
}:
let
  skill = name: builtins.readFile ../../skills/${name}/SKILL.md;
  common = [
    "goblins-spawn"
    "goblins-messaging"
    "goblins-docker"
    "goblins-raids"
  ];
in
{
  # Mounted in every sandbox of the configuration.
  always = lib.genAttrs (common ++ (if nix then [ "goblins-nix" ] else [ "goblins-packages" ])) skill;
  # Mounted only in sandboxes launched with a dev shell.
  devShell = lib.optionalAttrs (!nix) { goblins-devshell = skill "goblins-devshell"; };
  # Every skill Goblins manages, installed or not: injectedFiles cannot take
  # one of these names.
  managed = common ++ [
    "goblins-packages"
    "goblins-devshell"
    "goblins-nix"
  ];
}
