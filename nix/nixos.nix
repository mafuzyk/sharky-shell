# NixOS module for sharky. Follows the layout of nixpkgs' programs.fish and
# programs.zsh modules: install the package and register it as a permissible
# login shell. It deliberately does not set any users.users.<name>.shell.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.sharky;
in
{
  options.programs.sharky = {
    enable = lib.mkEnableOption "sharky, a Unix-first interactive shell";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalMD "the prebuilt static release binary shipped by the sharky flake (version 0.1.1-alpha)";
      description = ''
        The sharky package to install and register as a login shell.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
        message = "programs.sharky is only supported on x86_64-linux.";
      }
    ];

    environment.systemPackages = [ cfg.package ];

    environment.shells = [
      "/run/current-system/sw/bin/sharky"
      (lib.getExe cfg.package)
    ];
  };
}
