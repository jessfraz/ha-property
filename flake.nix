{
  description = "Property development environment and native CLI";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = {self, nixpkgs, rust-overlay, ...}: let
    systems = ["aarch64-darwin" "aarch64-linux" "x86_64-linux"];
    eachSystem = nixpkgs.lib.genAttrs systems;
    pkgsFor = system: import nixpkgs {
      inherit system;
      overlays = [(import rust-overlay)];
    };
  in {
    devShells = eachSystem (system: let pkgs = pkgsFor system; in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
          python314 uv cmake pkg-config sqlite jq just cargo-deny
          actionlint shellcheck nodejs
        ];
      };
    });
    packages = eachSystem (system: let
      pkgs = pkgsFor system;
      toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      platform = pkgs.makeRustPlatform {cargo = toolchain; rustc = toolchain;};
      cli = platform.buildRustPackage {
        pname = "ha-property";
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
        src = pkgs.lib.cleanSource ./.;
        cargoLock = {
          lockFile = ./Cargo.lock;
          outputHashes."sqlx-0.9.0" = "sha256-kLvUfUy6vmL9LYU7nzHv+iB6cIJm2CjDf9Yo6wpWrWg=";
          outputHashes."ha-app-ui-0.1.0" = "sha256-NC2ZxquF1CNaXXJo5QOrw4GruBtxcKRpDpVVxmXMC4c=";
        };
        cargoBuildFlags = ["--bin" "ha-property"];
        cargoTestFlags = ["--test" "cli"];
        __darwinAllowLocalNetworking = true;
        nativeBuildInputs = [pkgs.pkg-config pkgs.cmake];
        meta = {mainProgram = "ha-property"; license = pkgs.lib.licenses.mit; platforms = systems;};
      };
    in {ha-property = cli; default = cli;});
    apps = eachSystem (system: let app = {
      type = "app";
      meta.description = "Property tax ledger client";
      program = "${self.packages.${system}.ha-property}/bin/ha-property";
    }; in {ha-property = app; default = app;});
  };
}
