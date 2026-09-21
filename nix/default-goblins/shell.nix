{
  pkgs,
  mkGoblin,
  docker ? { },
}:
mkGoblin {
  inherit docker;
  pkg = pkgs.fish;
  binName = "fish";
  args = [
    "--interactive"
    "--init-command"
    "goblins completions fish | source"
  ];
}
