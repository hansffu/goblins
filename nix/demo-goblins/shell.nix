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
      "goblin completions fish | source"
    ];
  }
  // options
)
