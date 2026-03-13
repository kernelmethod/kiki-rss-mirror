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
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;

            buildInputs = [ ];
            nativeBuildInputs = [ pkgs.pkg-config ];
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          kiki = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });
        in
        {
          checks = {
            inherit kiki;
          };

          packages.default = kiki;

          devShells.default = craneLib.devShell {
            checks = self.checks.${system};

            packages = with pkgs; [
              cargo-deb
              cargo-generate-rpm
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
