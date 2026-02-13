{
  description = "AgentFS - A filesystem for AI agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      cargoToml = builtins.fromTOML (builtins.readFile ./cli/Cargo.toml);

      mkAgentfs =
        system:
        let
          pkgs = pkgsFor system;
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./cli/rust-toolchain.toml;
        in
        (pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        }).buildRustPackage
          {
            pname = cargoToml.package.name;
            inherit (cargoToml.package) version;
            src = ./.;

            cargoLock = {
              lockFile = ./cli/Cargo.lock;
              outputHashes = {
                "reverie-0.1.0" = "sha256-3Xj/q0UA957/WCfB1s0U/pwu0cnshFvbS4MwwrgrraU=";
              };
            };

            buildAndTestSubdir = "cli";
            postUnpack = "cp $sourceRoot/cli/Cargo.lock $sourceRoot/Cargo.lock";
            buildNoDefaultFeatures = !pkgs.stdenv.isLinux; # sandbox requires reverie (Linux-only)

            nativeBuildInputs = [ pkgs.pkg-config ];

            buildInputs =
              with pkgs;
              [
                openssl
              ]
              ++ lib.optionals stdenv.isLinux [
                fuse3
                libunwind
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                darwin.apple_sdk.frameworks.SystemConfiguration
              ];

            meta = {
              description = "The filesystem for agents";
              homepage = cargoToml.package.repository;
              license = pkgs.lib.licenses.mit;
              mainProgram = cargoToml.package.name;
            };
          };
    in
    {
      packages = forAllSystems (system: rec {
        agentfs = mkAgentfs system;
        default = agentfs;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./cli/rust-toolchain.toml;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = [
              rustToolchain
              pkgs.pkg-config
              pkgs.rust-analyzer
            ];

            buildInputs =
              with pkgs;
              [
                openssl
              ]
              ++ lib.optionals stdenv.isLinux [
                fuse3
                libunwind
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                darwin.apple_sdk.frameworks.SystemConfiguration
              ];

            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
