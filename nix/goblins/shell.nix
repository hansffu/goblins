{ pkgs, mkGoblin }:
mkGoblin {
  pkg = pkgs.fish;
  binName = "fish";
  roDirs = [ "$HOME/.config/fish" ];
  args = [
    "--interactive"
  ];
}
