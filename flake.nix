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

          # The default plugins written in Rust: the workspace in plugins/,
          # built as WebAssembly into $out/<name>.wasm. build.rs would build
          # them itself, but can't fetch their dependencies inside the Nix
          # sandbox, so it bundles these instead (through KIKI_PLUGINS_WASM_DIR
          # below). nixpkgs' rustc ships the standard library for
          # wasm32-unknown-unknown.
          wasmPlugins = let
            fs = pkgs.lib.fileset;
            # Each plugin directory under plugins/ that is also a crate,
            # without any target directory.
            crates = builtins.filter
              (name: builtins.pathExists (./plugins + "/${name}/Cargo.toml"))
              (builtins.attrNames (builtins.readDir ./plugins));
            src = fs.toSource {
              root = ./.;
              fileset = fs.unions ([
                ./plugins/Cargo.toml
                ./plugins/Cargo.lock
                ./sdk/rust/kiki-plugin/Cargo.toml
                ./sdk/rust/kiki-plugin/src
                ./sdk/rust/kiki-plugin/wit
                ./sdk/rust/kiki-plugin-macros/Cargo.toml
                ./sdk/rust/kiki-plugin-macros/src
              ] ++ builtins.concatMap (crate: [
                (./plugins + "/${crate}/Cargo.toml")
                (./plugins + "/${crate}/src")
              ]) crates);
            };
          in craneLib.mkCargoDerivation {
            pname = "kiki-rss-wasm-plugins";
            version = (craneLib.crateNameFromCargoToml { cargoToml = ./Cargo.toml; }).version;
            inherit src;
            cargoArtifacts = null;
            cargoVendorDir = craneLib.vendorCargoDeps {
              cargoLock = ./plugins/Cargo.lock;
            };
            postUnpack = ''
              cd $sourceRoot/plugins
              sourceRoot="."
            '';
            buildPhaseCargoCommand = ''
              cargo build --release --locked --workspace --target wasm32-unknown-unknown
            '';
            # plugins/<name>'s package is kiki-<name> (see build.rs).
            installPhaseCommand = ''
              for name in ${builtins.concatStringsSep " " crates}; do
                install -Dm644 "target/wasm32-unknown-unknown/release/kiki_''${name//-/_}.wasm" \
                  "$out/$name.wasm"
              done
            '';
            doInstallCargoArtifacts = false;
            strictDeps = true;
            # nixpkgs' rustc links WebAssembly with lld.
            nativeBuildInputs = [ pkgs.lld ];
          };

          commonArgs = {
            src = let
              sqlFilter = path: _type: builtins.match ".*\\.sql$" path != null;
              xmlFilter = path: _type: builtins.match ".*\\.xml$" path != null;
              # Markdown pulled into rustdoc via include_str! (e.g. src/docs/*.md)
              docsFilter = path: _type: builtins.match ".*/src/.*\\.md$" path != null;
              # Web UI pages and scripts pulled in via include_str!
              # (e.g. src/cli/web/*.html, src/cli/web/*.js)
              webUiFilter = path: _type: builtins.match ".*/src/.*\\.(html|js)$" path != null;
              # Bundled plugins, packed into a .tar.zst by build.rs and pulled
              # into tests via include_str! and include_bytes! (e.g.
              # plugins/filter/manifest.toml)
              pluginsFilter = path: _type: builtins.match ".*/plugins(/.*)?" path != null;
              # The WebAssembly plugin interface (sdk/rust/kiki-plugin/wit), read
              # by wasmtime's bindgen! (src/scripting/wasm.rs), and the plugin its tests run
              # (tests/wasm-fixture/fixture.wasm, via include_bytes!)
              witFilter = path: _type: builtins.match ".*/wit(/.*)?" path != null;
              wasmFixtureFilter = path: _type:
                builtins.match ".*/tests/wasm-fixture(/fixture\\.wasm)?" path != null;
              # The guide in book/ is built separately (see `book` below);
              # leaving it out means editing it doesn't rebuild the crate.
              notBook = path: path != toString ./book
                && !(pkgs.lib.hasPrefix (toString ./book + "/") path);
              customOrCargo = path: type: (notBook path) && (
                (sqlFilter path type) || (xmlFilter path type) || (docsFilter path type)
                || (webUiFilter path type) || (pluginsFilter path type)
                || (witFilter path type) || (wasmFixtureFilter path type)
                || (craneLib.filterCargoSources path type));
            in
              pkgs.lib.cleanSourceWith {
                src = ./.;
                filter = customOrCargo;
              };
            strictDeps = true;

            KIKI_PLUGINS_WASM_DIR = "${wasmPlugins}";

            # The test suite already runs in checks.tests; running it here too
            # roughly doubles the Nix build time (release + LTO).
            doCheck = false;

            buildInputs = [ pkgs.openssl ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.cacert ];
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          };

          # What the dependency-only builds (buildDepsOnly) take of `args`. They compile
          # Cargo.lock's crates against a dummy source and build script, so they need
          # neither the plugins written in Rust nor the crates in plugins/ and sdk/, which
          # aren't the main crate's dependencies. Leaving them out keeps the dependencies,
          # Wasmtime and Cranelift among them, cached when a plugin or the SDK changes.
          depsOnly = args: builtins.removeAttrs args [ "KIKI_PLUGINS_WASM_DIR" ] // {
            src = let
              notUnder = dir: path: path != toString dir
                && !(pkgs.lib.hasPrefix (toString dir + "/") path);
            in pkgs.lib.cleanSourceWith {
              inherit (args) src;
              filter = path: _type: notUnder ./plugins path && notUnder ./sdk path;
            };
          };

          cargoArtifacts = craneLib.buildDepsOnly (depsOnly commonArgs);

          kiki = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });

          # A release build that keeps its symbols, for perf and flamegraphs
          # ([profile.profiling] in Cargo.toml).
          profilingArgs = commonArgs // {
            pname = "kiki-rss-profiling";
            CARGO_PROFILE = "profiling";
            # The fixup phase would strip the symbols the profile keeps.
            dontStrip = true;
          };

          profiling = craneLib.buildPackage (profilingArgs // {
            cargoArtifacts = craneLib.buildDepsOnly (depsOnly profilingArgs);
          });

          # Dev-profile builds for the clippy and test checks, so they don't
          # pay for release optimizations and LTO.
          devArgs = commonArgs // {
            CARGO_PROFILE = "";
            # Line tables are enough for backtraces, and full debug info
            # noticeably slows down codegen.
            CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
          };

          devDeps = craneLib.buildDepsOnly (depsOnly devArgs);

          # Fully static musl binary for GitHub releases. crane cross-compiles
          # with pkgsStatic's build-platform rustc, which the binary cache
          # carries, and a musl C toolchain for the -sys crates.
          craneLibStatic = crane.mkLib pkgs.pkgsStatic;

          staticArgs = {
            inherit (commonArgs) src strictDeps doCheck KIKI_PLUGINS_WASM_DIR;
            # pkgsStatic adds -static to every link, including the glibc
            # build scripts, which then fail to link. rustc already links
            # musl binaries statically, so drop it.
            preBuild = "unset NIX_CFLAGS_LINK";
          };

          staticDeps = craneLibStatic.buildDepsOnly (depsOnly staticArgs);

          static = craneLibStatic.buildPackage (staticArgs // {
            cargoArtifacts = staticDeps;
          });

          # The static build with its symbols kept, like `profiling` above.
          # nixdev runs this, so the live service can be profiled with perf.
          staticProfilingArgs = staticArgs // {
            pname = "kiki-rss-static-profiling";
            CARGO_PROFILE = "profiling";
            dontStrip = true;
          };

          staticProfiling = craneLibStatic.buildPackage (staticProfilingArgs // {
            cargoArtifacts = craneLibStatic.buildDepsOnly (depsOnly staticProfilingArgs);
          });

          # The user guide (an mdBook in book/) under $out/guide, with the
          # landing page in book/landing/ in front of it. kiki-publish puts
          # the rustdoc, API reference and coverage report beside them, in
          # docs/, api/ and coverage/.
          book = pkgs.stdenvNoCC.mkDerivation {
            pname = "kiki-rss-book";
            version = (craneLib.crateNameFromCargoToml { cargoToml = ./Cargo.toml; }).version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                (pkgs.lib.fileset.difference ./book (pkgs.lib.fileset.maybeMissing ./book/build))
                # Included by book/src/writing-plugins.md.
                ./src/docs/scripting.md
              ];
            };
            # lychee builds its HTTP client even when offline, and needs CA
            # certificates to do so.
            nativeBuildInputs = [ pkgs.mdbook pkgs.lychee pkgs.cacert ];
            buildPhase = ''
              runHook preBuild
              mdbook build book --dest-dir "$out/guide"
              cp book/landing/* "$out/"
              runHook postBuild
            '';
            # Check every link within the site, anchors included. Links to
            # the reports kiki-publish adds are skipped, as are external
            # links, which would need the network, and mdBook's 404 page,
            # whose links assume the site is served from the root.
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              lychee --offline --include-fragments --no-progress \
                --exclude "^file://$out/(api|docs|coverage)(/|$)" \
                --exclude-path "$out/guide/404.html" \
                "$out"
              runHook postCheck
            '';
            dontInstall = true;
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
            inherit kiki book;

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
              # Many tests spend part of their time waiting on a test server
              # or the filesystem, so running twice as many as there are
              # cores keeps the CPU busy. More than that is slower again.
              preBuild = ''
                cores=''${NIX_BUILD_CORES:-0}
                if [ "$cores" -le 0 ]; then cores=$(nproc); fi
                export RUST_TEST_THREADS=$((cores * 2))
              '';
            });
          };

          packages = {
            default = kiki;
            wasm-plugins = wasmPlugins;
            inherit book docs coverage profiling;
          } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
            inherit static;
            static-profiling = staticProfiling;
          };

          devShells.default = craneLib.devShell {
            checks = self.checks.${system};

            packages = with pkgs; [
              mdbook
              # For build.rs to link the plugins written in Rust (see wasmPlugins)
              lld
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
