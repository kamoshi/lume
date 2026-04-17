{
  description = "Lume – a statically-typed functional language (compiler + LSP)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        inherit (pkgs) lib;

        # ── Source filtering ────────────────────────────────────────────
        # Include Cargo sources *and* the std/ directory (embedded via
        # include_str! at compile time).
        craneLibForFilter = crane.mkLib pkgs;
        src = lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (lib.hasInfix "/std/" path)
            || (lib.hasInfix "/tests/" path)
            || (craneLibForFilter.filterCargoSources path type);
        };

        # ── Native toolchain ───────────────────────────────────────────
        nativeToolchain = fenix.packages.${system}.stable.withComponents [
          "cargo" "rustc" "rust-std" "clippy" "rustfmt"
        ];
        nativeCrane = (crane.mkLib pkgs).overrideToolchain nativeToolchain;

        commonArgs = {
          inherit src;
          pname = "lume-workspace";
          version = "0.1.0";
          cargoExtraArgs = "--workspace --exclude lume-wasm";
        };

        nativeArtifacts = nativeCrane.buildDepsOnly commonArgs;

        # ── Cross-compilation helper ───────────────────────────────────
        mkCross =
          { rustTarget
          , depsBuildBuild ? [ ]
          , extraEnv ? { }
          }:
          let
            toolchain = fenix.packages.${system}.combine [
              fenix.packages.${system}.stable.cargo
              fenix.packages.${system}.stable.rustc
              fenix.packages.${system}.targets.${rustTarget}.stable.rust-std
            ];
            crossCrane = (crane.mkLib pkgs).overrideToolchain toolchain;
            baseArgs = commonArgs // {
              CARGO_BUILD_TARGET = rustTarget;
              HOST_CC = "${pkgs.stdenv.cc.nativePrefix}cc";
              inherit depsBuildBuild;
            } // extraEnv;
            crossArtifacts = crossCrane.buildDepsOnly baseArgs;
          in
          {
            lume = crossCrane.buildPackage (baseArgs // {
              cargoArtifacts = crossArtifacts;
              cargoExtraArgs = "--package lume";
            });
            lume-lsp = crossCrane.buildPackage (baseArgs // {
              cargoArtifacts = crossArtifacts;
              cargoExtraArgs = "--package lume-lsp";
            });
          };

        # ── Per-target cross builds ────────────────────────────────────
        x86_64-linux = mkCross {
          rustTarget = "x86_64-unknown-linux-musl";
          depsBuildBuild = [ pkgs.pkgsCross.musl64.stdenv.cc ];
          extraEnv.CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER =
            "${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}cc";
        };

        aarch64-linux = mkCross {
          rustTarget = "aarch64-unknown-linux-musl";
          depsBuildBuild = [ pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc ];
          extraEnv.CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER =
            "${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc.targetPrefix}cc";
        };

        x86_64-windows = mkCross {
          rustTarget = "x86_64-pc-windows-gnu";
          depsBuildBuild = [ pkgs.pkgsCross.mingwW64.stdenv.cc ];
          extraEnv.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER =
            "${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}cc";
        };
      in
      {
        # ── Packages ─────────────────────────────────────────────────────
        packages =
          {
            # Native (host) builds
            lume = nativeCrane.buildPackage (commonArgs // {
              cargoArtifacts = nativeArtifacts;
              cargoExtraArgs = "--package lume";
            });
            lume-lsp = nativeCrane.buildPackage (commonArgs // {
              cargoArtifacts = nativeArtifacts;
              cargoExtraArgs = "--package lume-lsp";
            });
            default = self.packages.${system}.lume;

            # Cross: Linux (static musl)
            lume-x86_64-linux = x86_64-linux.lume;
            lume-lsp-x86_64-linux = x86_64-linux.lume-lsp;
            lume-aarch64-linux = aarch64-linux.lume;
            lume-lsp-aarch64-linux = aarch64-linux.lume-lsp;

            # Cross: Windows
            lume-x86_64-windows = x86_64-windows.lume;
            lume-lsp-x86_64-windows = x86_64-windows.lume-lsp;
          }
          # Cross: macOS other arch (only available on macOS hosts)
          // lib.optionalAttrs (system == "aarch64-darwin")
            (let cross = mkCross { rustTarget = "x86_64-apple-darwin"; }; in {
              lume-x86_64-darwin = cross.lume;
              lume-lsp-x86_64-darwin = cross.lume-lsp;
            })
          // lib.optionalAttrs (system == "x86_64-darwin")
            (let cross = mkCross { rustTarget = "aarch64-apple-darwin"; }; in {
              lume-aarch64-darwin = cross.lume;
              lume-lsp-aarch64-darwin = cross.lume-lsp;
            });

        # ── Checks ───────────────────────────────────────────────────────
        checks = {
          workspace-clippy = nativeCrane.cargoClippy (commonArgs // {
            cargoArtifacts = nativeArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
          workspace-test = nativeCrane.cargoTest (commonArgs // {
            cargoArtifacts = nativeArtifacts;
          });
          workspace-fmt = nativeCrane.cargoFmt { inherit src; };
        };

        # ── Dev shell ────────────────────────────────────────────────────
        devShells.default = nativeCrane.devShell {
          checks = self.checks.${system};
          packages = [ pkgs.rust-analyzer ];
        };
      }
    );
}
