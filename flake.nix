{
  description = "Poly — a Wayland compositor with JavaScript-scriptable layouts";

  # libdisplay-info 0.4 is too new for libdisplay-info-sys 0.3 (which requires
  # < 0.4), so the flake tracks the release branch that still ships 0.3.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Build-time and run-time libraries, as listed in docs/building.md. Mesa
      # and libglvnd are only needed at run time: Smithay loads EGL/GLES through
      # libloading, and the buffers come from the GPU driver.
      nativeDependencies =
        pkgs:
        with pkgs;
        [
          wayland
          wayland-protocols
          wayland-scanner
          libinput
          systemd
          seatd
          libgbm
          libdrm
          libxkbcommon
          libdisplay-info
        ];

      runtimeDependencies =
        pkgs:
        with pkgs;
        [
          mesa
          libglvnd
        ];

      allDependencies = pkgs: nativeDependencies pkgs ++ runtimeDependencies pkgs;
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          inherit (pkgs.stdenv.hostPlatform) system;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "poly";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

            src = self;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # Smithay is pinned to a git revision. `nix flake prefetch` on
              # that revision produces these hashes; update them together with
              # both Git dependencies in the workspace Cargo.toml.
              outputHashes = {
                "smithay-0.7.0" = "sha256-GMKOTa0yqYXo5nOpsjYJESUnOobAVh0TW7md2CMezIE=";
                "smithay-drm-extras-0.1.0" = "sha256-GMKOTa0yqYXo5nOpsjYJESUnOobAVh0TW7md2CMezIE=";
              };
            };

            # The workspace also contains the capture benchmark. Only the
            # compositor belongs to this package.
            cargoBuildFlags = [
              "-p"
              "poly"
            ];

            nativeBuildInputs = with pkgs; [
              pkg-config
              autoPatchelfHook
            ];

            buildInputs = allDependencies pkgs;

            # Wayland, EGL and the GPU driver are loaded at run time.
            autoPatchelfIgnoreMissingDeps = false;

            doCheck = false;

            meta = {
              description = "A Wayland compositor with JavaScript-scriptable layouts";
              homepage = "https://github.com/Dudly-wayland-project/poly-wm";
              license = pkgs.lib.licenses.gpl3Plus;
              mainProgram = "poly";
              platforms = pkgs.lib.platforms.linux;
              sourceProvenance = [ pkgs.lib.sourceTypes.fromSource ];
            };
          };
        }
      );

      devShells = forAllSystems (
        pkgs:
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              pkg-config
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
            ];

            buildInputs = allDependencies pkgs;

            # The nested and native backends need the same libraries at run
            # time, including the EGL/GLES provider.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (allDependencies pkgs);

            shellHook = ''
              echo "Poly development shell"
              echo "  rustc: $(rustc --version)"
              echo "  cargo: $(cargo --version)"
            '';
          };
        }
      );

      apps = forAllSystems (
        pkgs:
        {
          default = {
            type = "app";
            program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/poly";
            meta.description = "Run the Poly compositor";
          };
        }
      );
    };
}
