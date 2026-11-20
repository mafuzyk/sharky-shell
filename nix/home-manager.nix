# Home Manager module for sharky. Installs the package and, only when asked,
# manages the user's configuration files under $XDG_CONFIG_HOME/sharky.
# File management is opt-in so an existing hand-written config is never
# clobbered: leave the *_File options at their `null` default and sharky keeps
# reading whatever is already there.
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
      defaultText = lib.literalMD "the prebuilt static release binary shipped by the sharky flake (version 3.6.0)";
      description = ''
        The sharky package to install.
      '';
    };

    configFile = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      example = lib.literalMD ''
        ```sh
        tide history 10000
        pearl EDITOR "nvim"
        reef ll "ls -lah"
        ```
      '';
      description = ''
        Contents of {file}`$XDG_CONFIG_HOME/sharky/config.sharky`.
        `null` (the default) leaves `~/.config/sharky/` untouched, so
        Home Manager never overwrites a configuration written by hand.
      '';
    };

    themeFile = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      example = lib.literalMD ''
        ```sh
        coral sea #35B8C5
        wave prompt "{cwd} ❯ "
        ```
      '';
      description = ''
        Contents of {file}`$XDG_CONFIG_HOME/sharky/theme.sharky`.
        `null` (the default) leaves `~/.config/sharky/` untouched.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions = [
          {
            assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
            message = "programs.sharky is only supported on x86_64-linux.";
          }
        ];
      }

      { home.packages = [ cfg.package ]; }

      (lib.mkIf (cfg.configFile != null) {
        xdg.configFile."sharky/config.sharky".text = cfg.configFile;
      })

      (lib.mkIf (cfg.themeFile != null) {
        xdg.configFile."sharky/theme.sharky".text = cfg.themeFile;
      })
    ]
  );
}
