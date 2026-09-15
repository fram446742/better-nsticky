{
  lib,
  rustPlatform,
}:

let
  cargoToml = fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "Sticky and staged window management for the niri compositor (better-nsticky)";
    homepage = "https://github.com/fram446742/better-nsticky";
    mainProgram = "nsticky";
    license = lib.licenses.bsd3;
    maintainers = with lib.maintainers; [ lonerOrz ];
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];
  };
}
