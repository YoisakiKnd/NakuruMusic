{
  description = "NakuruMusic, a lightweight YouTube Music TUI with built-in playback";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  in {
    packages = forAllSystems (pkgs: let
      package = pkgs.rustPlatform.buildRustPackage {
        pname = manifest.package.name;
        version = manifest.package.version;

        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [./Cargo.toml ./Cargo.lock ./src ./vendor];
        };
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [pkgs.pkg-config pkgs.makeWrapper];
        buildInputs = [pkgs.alsa-lib pkgs.openssl];

        doCheck = true;
        cargoTestFlags = ["--all-targets"];

        postFixup = ''
          wrapProgram "$out/bin/nakuru-music" \
            --suffix PATH : ${pkgs.lib.makeBinPath [pkgs.xdg-utils]}
        '';

        meta = {
          inherit (manifest.package) description;
          homepage = "https://github.com/YoisakiKnd/NakuruMusic";
          license = pkgs.lib.licenses.gpl3Only;
          mainProgram = "nakuru-music";
          platforms = systems;
        };
      };
    in {
      default = package;
      nakuru-music = package;
    });

    apps = forAllSystems (pkgs: {
      default = {
        type = "app";
        program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/nakuru-music";
        meta = self.packages.${pkgs.stdenv.hostPlatform.system}.default.meta;
      };
    });

    devShells = forAllSystems (pkgs: {
      default = pkgs.mkShell {
        inputsFrom = [self.packages.${pkgs.stdenv.hostPlatform.system}.default];
        packages = [pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy];
      };
    });

    checks = forAllSystems (pkgs: {
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    });
  };
}
