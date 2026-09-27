{
  pkgs,
  mkGoblin,
  options ? { },
}:
mkGoblin (
  {
    pkg = pkgs.fish;
    binName = "fish";
    args = [
      "--interactive"
      "--init-command"
      "goblins completions fish | source"
    ];
  }
  // options
)
