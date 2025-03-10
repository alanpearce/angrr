let
  sources = import ./npins;
  pkgs = import sources.nixpkgs { };
  craneLib = import sources.crane { inherit pkgs; };
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
craneLib.buildPackage (
  commonArgs
    // {
    postInstall = ''
      installShellCompletion --cmd angrr \
        --bash <($out/bin/angrr completion bash) \
        --fish <($out/bin/angrr completion fish) \
        --zsh <($out/bin/angrr completion zsh)
    '';
  }
)
