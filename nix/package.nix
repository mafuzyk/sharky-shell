# Prebuilt static release binary of sharky. Shared by the flake outputs and by
# both modules' `package` default, so consumers can override with a source build.
{
  lib,
  stdenv,
  fetchurl,
}:

stdenv.mkDerivation (finalAttrs: {
  pname = "sharky";
  version = "3.6.0";

  src = fetchurl {
    url = "https://github.com/mafuzyk/sharky-shell/releases/download/v${finalAttrs.version}/sharky-x86_64-unknown-linux-musl.tar.gz";
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
    install -Dm644 ${../LICENSE} $out/share/licenses/sharky/LICENSE
    runHook postInstall
  '';

  # Required by NixOS for `users.users.<name>.shell` (types.shellPackage);
  # without it the shell is rejected as "not a shell package".
  passthru.shellPath = "/bin/sharky";

  meta = {
    description = "Unix-first interactive shell written in Rust (prebuilt static binary)";
    homepage = "https://github.com/mafuzyk/sharky-shell";
    license = lib.licenses.gpl3Only;
    platforms = [ "x86_64-linux" ];
    mainProgram = "sharky";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
})
