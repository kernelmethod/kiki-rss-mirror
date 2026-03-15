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
              customOrCargo = path: type:
                (sqlFilter path type) || (xmlFilter path type) || (craneLib.filterCargoSources path type);
            in
              pkgs.lib.cleanSourceWith {
                src = ./.;
                filter = customOrCargo;
              };
            strictDeps = true;

            buildInputs = [ pkgs.openssl ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.cacert ];
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
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
