{ pkgs, mkGoblin }:
mkGoblin {
  pkg = pkgs.fish;
  binName = "fish";
  args = [
    "--no-config"
    "--interactive"
  ];
}
