{
  description = "sharky - Unix-first interactive shell (prebuilt static binary)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      sharky = pkgs.callPackage ./nix/package.nix { };
    in
    {
      packages.${system} = {
        default = sharky;
        sharky = sharky;
        sharky-bin = sharky;
      };

      # Rust toolchain matching rust/Cargo.toml (edition 2024, rust-version
      # 1.85); the locked nixpkgs currently ships rustc 1.99.1. The linker is
      # the stdenv default C compiler, which is all the crate needs.
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          pkgs.cargo
          pkgs.clippy
          pkgs.rustc
          pkgs.rustfmt
        ];
      };

      nixosModules = {
        default = ./nix/nixos.nix;
        sharky = ./nix/nixos.nix;
      };

      homeManagerModules = {
        default = ./nix/home-manager.nix;
        sharky = ./nix/home-manager.nix;
      };
    };
}
