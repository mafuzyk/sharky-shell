{
  description = "sharky - Unix-first interactive shell (prebuilt static binary)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      version = "3.6.0";
      sharky = pkgs.stdenv.mkDerivation {
        pname = "sharky";
        inherit version;

        src = pkgs.fetchurl {
          url =
            "https://github.com/mafuzyk/sharky-shell/releases/download/v${version}/sharky-x86_64-unknown-linux-musl.tar.gz";
          hash = "sha256-xxUIj/4OM/ZoG4CJ8rCY+e/S58ICB+LRBYDtV0BnNiU=";
        };

        # The tarball has no top-level directory and carries a prebuilt binary.
        unpackPhase = ''
          runHook preUnpack
          tar -xzf $src
          runHook postUnpack
        '';
        dontBuild = true;

        installPhase = ''
          runHook preInstall
          install -Dm755 sharky $out/bin/sharky
          install -Dm644 sharky.1 $out/share/man/man1/sharky.1
          install -Dm644 config.sharky $out/share/sharky/config.sharky
          install -Dm644 theme.sharky $out/share/sharky/theme.sharky
          install -Dm644 ${./LICENSE} $out/share/licenses/sharky/LICENSE
          runHook postInstall
        '';

        meta = {
          description =
            "Unix-first interactive shell written in Rust (prebuilt static binary)";
          homepage = "https://github.com/mafuzyk/sharky-shell";
          license = pkgs.lib.licenses.gpl3Only;
          platforms = [ "x86_64-linux" ];
          mainProgram = "sharky";
          sourceProvenance = [ pkgs.lib.sourceTypes.binaryNativeCode ];
        };
      };
    in
    {
      packages.${system} = {
        default = sharky;
        sharky = sharky;
        sharky-bin = sharky;
      };
    };
}
