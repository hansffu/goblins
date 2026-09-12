{
  description = "Goblins: sandboxed shells with approved live Nix package grants";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/21a67dc470149f337cecafbe965d8d252a390518";
    agent-sandbox = {
      url = "github:archie-judd/agent-sandbox.nix/566defa1560051c4e48049389f942a6c1a53aee9";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  # Keep the standalone experiment reproducible while exposing the same native
  # Nix package, app and development shell at the repository root.
  outputs = inputs: (import ./spikes/live-package-grant/flake.nix).outputs inputs;
}
