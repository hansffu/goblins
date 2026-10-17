# Scopes of the demo goblins. goblins-dev keeps its Cargo and Nix fetcher
# caches here between sessions; its claude and codex children join it.
{ mkScope }:
{
  goblins-dev = mkScope {
    storage = {
      cargo.path = "$HOME/.cargo";
      nix-cache.path = "$HOME/.cache/nix";
    };
  };
}
