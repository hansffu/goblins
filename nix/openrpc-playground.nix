{ pkgs }:
pkgs.stdenvNoCC.mkDerivation {
  pname = "openrpc-playground";
  version = "1.1.2";
  src = pkgs.fetchurl {
    url = "https://registry.npmjs.org/@open-rpc/playground/-/playground-1.1.2.tgz";
    hash = "sha256-MFNU+66F6k2gka+i88P+CS87m2Mzdvw93yXZMQbR4Sg=";
  };
  sourceRoot = "package/dist";
  dontBuild = true;
  installPhase = ''
    mkdir -p "$out"
    cp -r . "$out/"
    substituteInPlace "$out/index.html" --replace-fail '/src/favicon.svg' '/favicon.ico'
  '';
  meta.license = pkgs.lib.licenses.asl20;
}
