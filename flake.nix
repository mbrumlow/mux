{
  description = "mux – terminal multiplexer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";

    claude-code.url = "github:sadjow/claude-code-nix";
  };

  outputs = { self, nixpkgs, fenix, crane, flake-utils, claude-code }:
    (flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ claude-code.overlays.default ];
        };

        f = fenix.packages.${system};
        toolchain = f.combine [
          f.stable.rustc
          f.stable.cargo
          f.stable.clippy
          f.stable.rustfmt
          f.stable.rust-src
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          pname = "mux";
          version = "0.1.0";
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        mux = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });
      in {
        packages.default = mux;

        devShells.default = craneLib.devShell {
          inputsFrom = [ mux ];
          packages = [
            pkgs.rust-analyzer
            pkgs.claude-code
          ];
        };
      }
    )) // {
      overlays.default = final: prev: {
        mux = self.packages.${final.system}.default;
      };
    };
}
