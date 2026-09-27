{
  description = "Kiki RSS feed aggregator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, flake-utils, ... }:
    let
      perSystem = flake-utils.lib.eachDefaultSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;

          commonArgs = {
            src = let
              sqlFilter = path: _type: builtins.match ".*\\.sql$" path != null;
              xmlFilter = path: _type: builtins.match ".*\\.xml$" path != null;
              # Markdown pulled into rustdoc via include_str! (e.g. src/docs/*.md)
              docsFilter = path: _type: builtins.match ".*/src/.*\\.md$" path != null;
              customOrCargo = path: type:
                (sqlFilter path type) || (xmlFilter path type) || (docsFilter path type)
                || (craneLib.filterCargoSources path type);
            in
              pkgs.lib.cleanSourceWith {
                src = ./.;
                filter = customOrCargo;
              };
            strictDeps = true;

            # The test suite already runs in checks.tests; running it here too
            # roughly doubles the Nix build time (release + LTO).
            doCheck = false;

            buildInputs = [ pkgs.openssl ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.cacert ];
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          kiki = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });

          # Dev-profile builds for the clippy and test checks, so they don't
          # pay for release optimizations and LTO.
          devArgs = commonArgs // {
            CARGO_PROFILE = "";
            # Line tables are enough for backtraces, and full debug info
            # noticeably slows down codegen.
            CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
          };

          devDeps = craneLib.buildDepsOnly devArgs;

          # Fully static musl binary for GitHub releases. crane cross-compiles
          # with pkgsStatic's build-platform rustc, which the binary cache
          # carries, and a musl C toolchain for the -sys crates.
          craneLibStatic = crane.mkLib pkgs.pkgsStatic;

          static = craneLibStatic.buildPackage {
            inherit (commonArgs) src strictDeps doCheck;
            # pkgsStatic adds -static to every link, including the glibc
            # build scripts, which then fail to link. rustc already links
            # musl binaries statically, so drop it.
            preBuild = "unset NIX_CFLAGS_LINK";
          };

          # rustdoc for the kiki_rss crate, including the guides pulled in
          # from src/docs/*.md. The HTML lands in $out/share/doc.
          docs = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });

          coverageArgs = commonArgs // {
            # Build with the dev profile, like `cargo test` in CI.
            CARGO_PROFILE = "";
            # cargo-llvm-cov needs the LLVM tools matching rustc's LLVM.
            LLVM_COV = "${pkgs.rustc.unwrapped.llvmPackages.llvm}/bin/llvm-cov";
            LLVM_PROFDATA = "${pkgs.rustc.unwrapped.llvmPackages.llvm}/bin/llvm-profdata";
          };

          # Record source paths relative to the repo root rather than the Nix
          # build directory, so the reports line up with the checkout.
          coverageLlvmCovArgs = "--no-report --remap-path-prefix";

          # Instrumented builds can't reuse the release-profile dependency
          # artifacts, so cache a separate set of dependencies compiled through
          # cargo-llvm-cov. Its RUSTFLAGS and target dir must match the
          # coverage build exactly, or cargo will rebuild everything anyway.
          coverageDeps = craneLib.buildDepsOnly (coverageArgs // {
            pname = "kiki-rss-llvm-cov";
            nativeBuildInputs = coverageArgs.nativeBuildInputs ++ [ pkgs.cargo-llvm-cov ];
            buildPhaseCargoCommand = ''
              cargoWithProfile llvm-cov test --locked ${coverageLlvmCovArgs}
            '';
          });

          # Test coverage report: $out/lcov.info for coverage services,
          # $out/html for browsing, and $out/summary.txt for build logs.
          coverage = craneLib.cargoLlvmCov (coverageArgs // {
            cargoArtifacts = coverageDeps;
            cargoLlvmCovExtraArgs = coverageLlvmCovArgs;
            postBuild = ''
              mkdir -p $out
              cargo llvm-cov report --remap-path-prefix --lcov --output-path $out/lcov.info
              cargo llvm-cov report --remap-path-prefix --html --output-dir $out
              cargo llvm-cov report --remap-path-prefix --summary-only | tee $out/summary.txt
            '';
          });
        in
        {
          checks = {
            inherit kiki;

            fmt = craneLib.cargoFmt {
              inherit (commonArgs) src;
            };

            clippy = craneLib.cargoClippy (devArgs // {
              cargoArtifacts = devDeps;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            });

            tests = craneLib.cargoTest (devArgs // {
              cargoArtifacts = devDeps;
              doCheck = true;
            });
          };

          packages = {
            default = kiki;
            inherit docs coverage;
          } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
            inherit static;
          };

          devShells.default = craneLib.devShell {
            checks = self.checks.${system};

            packages = with pkgs; [
              cargo-deb
            ];
          };
        }
      );
    in
    perSystem // {
      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.kiki.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };
    };
}
