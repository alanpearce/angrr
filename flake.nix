{
  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";

    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";

    crane.url = "github:ipetkov/crane";
    crane.inputs.nixpkgs.follows = "nixpkgs";

    flake-compat.url = "github:edolstra/flake-compat";
    flake-compat.flake = false;
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      {
        self,
        inputs,
        lib,
        ...
      }:
      {
        systems = [
          "x86_64-linux"
          "aarch64-linux"
        ];
        imports = [
          inputs.flake-parts.flakeModules.easyOverlay
          inputs.treefmt-nix.flakeModule
        ];
        flake = {
          nixosModules.angrr = ./nixos/module.nix;
        };
        perSystem =
          {
            config,
            self',
            pkgs,
            system,
            ...
          }:
          let
            craneLib = inputs.crane.lib.${system};
            src = craneLib.cleanCargoSource (craneLib.path ./.);
            bareCommonArgs = {
              inherit src;
              nativeBuildInputs = with pkgs; [ installShellFiles ];
              buildInputs = [ ];
            };
            cargoArtifacts = craneLib.buildDepsOnly bareCommonArgs;
            commonArgs = bareCommonArgs // {
              inherit cargoArtifacts;
            };
          in
          {
            packages = {
              angrr = craneLib.buildPackage (
                commonArgs
                // {
                  postInstall = ''
                    installShellCompletion --cmd angrr \
                      --bash <($out/bin/angrr completion bash) \
                      --fish <($out/bin/angrr completion fish) \
                      --zsh  <($out/bin/angrr completion zsh)
                  '';
                }
              );
              default = config.packages.angrr;
            };
            overlayAttrs.angrr = config.packages.angrr;
            checks = {
              inherit (self'.packages) angrr;
              doc = craneLib.cargoDoc commonArgs;
              fmt = craneLib.cargoFmt { inherit src; };
              nextest = craneLib.cargoNextest commonArgs;
              clippy = craneLib.cargoClippy (
                commonArgs // { cargoClippyExtraArgs = "--all-targets -- --deny warnings"; }
              );
            };
            treefmt = {
              projectRootFile = "flake.nix";
              programs = {
                nixfmt-rfc-style.enable = true;
                rustfmt.enable = true;
                shfmt.enable = true;
              };
            };
            devShells.default = pkgs.mkShell {
              inputsFrom = lib.attrValues self'.checks;
              packages = with pkgs; [
                rustup
                rust-analyzer
              ];
            };
          };
      }
    );
}
