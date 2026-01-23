{
  lib,
  rustPlatform,
  pkg-config,
  wayland,
  libxkbcommon,
  udev,
  seatd,
  libinput,
  libgbm,
  libdisplay-info_0_2,
  gitRev ? null,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "projectwc";
  version =
    if gitRev != null
    then lib.substring 0 8 gitRev
    else "dev";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [pkg-config];

  buildInputs = [
    wayland
    libxkbcommon
    udev
    seatd
    libinput
    libgbm
    libdisplay-info_0_2 # TODO: update to 0.3
  ];

  doCheck = false;

  meta = {
    description = "Wayland compositor written in Rust using smithay";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
    mainProgram = "projectwc";
  };
})
