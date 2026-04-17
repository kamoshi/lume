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

  outputs =
    {
      self,
      nixpkgs,
      crane,
      fenix,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        # Separate import needed to reference Windows target libraries
        # (e.g. windows.pthreads) whose meta.platforms = windows-only.
        pkgsUnsupported = import nixpkgs {
          inherit system;
          config.allowUnsupportedSystem = true;
        };
        inherit (pkgs) lib;

        # ── Source filtering ────────────────────────────────────────────
        # Include Cargo sources *and* the std/ directory (embedded via
        # include_str! at compile time).
        craneLibForFilter = crane.mkLib pkgs;
        src = lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            (lib.hasInfix "/std/" path)
            || (lib.hasInfix "/tests/" path)
            || (craneLibForFilter.filterCargoSources path type);
        };

        # ── Native toolchain ───────────────────────────────────────────
        nativeToolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "rustc"
          "rust-std"
          "clippy"
          "rustfmt"
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
          {
            rustTarget,
            depsBuildBuild ? [ ],
            extraEnv ? { },
          }:
          let
            toolchain = fenix.packages.${system}.combine [
              fenix.packages.${system}.stable.cargo
              fenix.packages.${system}.stable.rustc
              fenix.packages.${system}.targets.${rustTarget}.stable.rust-std
            ];
            crossCrane = (crane.mkLib pkgs).overrideToolchain toolchain;
            baseArgs =
              commonArgs
              // {
                CARGO_BUILD_TARGET = rustTarget;
                HOST_CC = "${pkgs.stdenv.cc.nativePrefix}cc";
                # Tests can't run on the host when cross-compiling.
                doCheck = false;
                inherit depsBuildBuild;
              }
              // extraEnv;
            crossArtifacts = crossCrane.buildDepsOnly baseArgs;
          in
          {
            lume = crossCrane.buildPackage (
              baseArgs
              // {
                cargoArtifacts = crossArtifacts;
                cargoExtraArgs = "--package lume";
                postInstall = ''
                  for f in "$out/bin/"*; do
                    base="$(basename "$f")"
                    name="''${base%%.*}"
                    ext="''${base#"$name"}"
                    mv "$f" "$out/bin/$name.${rustTarget}$ext"
                  done
                '';
              }
            );
            lume-lsp = crossCrane.buildPackage (
              baseArgs
              // {
                cargoArtifacts = crossArtifacts;
                cargoExtraArgs = "--package lume-lsp";
                postInstall = ''
                  for f in "$out/bin/"*; do
                    base="$(basename "$f")"
                    name="''${base%%.*}"
                    ext="''${base#"$name"}"
                    mv "$f" "$out/bin/$name.${rustTarget}$ext"
                  done
                '';
              }
            );
          };

        # ── Per-target cross builds ────────────────────────────────────
        # LuaJIT (vendored via mlua) unconditionally redirects fopen →
        # fopen64 etc. on __linux__.  musl libc does not provide the *64
        # symbols; map them back to the standard names at the preprocessor
        # level and disable glibc-only _FORTIFY_SOURCE.
        muslCflags = builtins.concatStringsSep " " [
          "-Dfopen64=fopen"
          "-Dfseeko64=fseeko"
          "-Dftello64=ftello"
          "-Dtmpfile64=tmpfile"
          "-Dmkstemp64=mkstemp"
          "-Dmmap64=mmap"
          "-U_FORTIFY_SOURCE"
        ];

        x86_64-linux = mkCross {
          rustTarget = "x86_64-unknown-linux-musl";
          depsBuildBuild = [ pkgs.pkgsCross.musl64.stdenv.cc ];
          extraEnv.CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}cc";
          extraEnv.CC_x86_64_unknown_linux_musl = "${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}cc";
          extraEnv.AR_x86_64_unknown_linux_musl = "${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}ar";
          extraEnv.CFLAGS_x86_64_unknown_linux_musl = muslCflags;
        };

        # NOTE: Windows cross-compilation (x86_64-pc-windows-gnu) requires
        # a working mingw toolchain.  On current nixpkgs-unstable the GCC
        # bootstrap fails on macOS.  Build from a Linux host or CI instead.
        x86_64-windows = mkCross {
          rustTarget = "x86_64-pc-windows-gnu";
          depsBuildBuild = [ pkgs.pkgsCross.mingwW64.stdenv.cc ];
          extraEnv.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}cc";
          # mlua's vendored LuaJIT build uses the `cc` crate to find the
          # cross-compiler. Without this, it falls back to native GCC and
          # the LuaJIT Makefile rejects the mismatched TARGET_SYS=Windows.
          extraEnv.CC_x86_64_pc_windows_gnu = "${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}cc";
          extraEnv.AR_x86_64_pc_windows_gnu = "${pkgs.pkgsCross.mingwW64.stdenv.cc.targetPrefix}ar";
          # Rust's stdlib for x86_64-pc-windows-gnu links -l:libpthread.a
          # (winpthreads). windows.pthreads is a Windows-only package so we
          # must reference it via pkgsUnsupported to bypass the platform check.
          extraEnv.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L ${pkgsUnsupported.pkgsCross.mingwW64.windows.pthreads}/lib";
        };

      in
      {
        # ── Packages ─────────────────────────────────────────────────────
        packages = {
          # Native (host) builds
          lume = nativeCrane.buildPackage (
            commonArgs
            // {
              cargoArtifacts = nativeArtifacts;
              cargoExtraArgs = "--package lume";
            }
          );
          lume-lsp = nativeCrane.buildPackage (
            commonArgs
            // {
              cargoArtifacts = nativeArtifacts;
              cargoExtraArgs = "--package lume-lsp";
            }
          );
          default = self.packages.${system}.lume;

          # Cross: Linux (static musl)
          lume-x86_64-linux = x86_64-linux.lume;
          lume-lsp-x86_64-linux = x86_64-linux.lume-lsp;

          # Cross: Windows
          lume-x86_64-windows = x86_64-windows.lume;
          lume-lsp-x86_64-windows = x86_64-windows.lume-lsp;
        };

        # ── Checks ───────────────────────────────────────────────────────
        checks = {
          workspace-clippy = nativeCrane.cargoClippy (
            commonArgs
            // {
              cargoArtifacts = nativeArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          workspace-test = nativeCrane.cargoTest (
            commonArgs
            // {
              cargoArtifacts = nativeArtifacts;
            }
          );
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
