{
  description = "Logos monero_wallet_backend — Monero wallet coordinator (registry, sync, balances, history, send orchestration).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    # Local paths until the repos are published (P8 switches these to github:). Every
    # dependency follows THIS module-builder: a skewed generated ABI segfaults in provider init.
    monero_node_module = {
      url = "path:/Users/dlipicar/repos/logos-monero-node-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    monero_wallet_core_module = {
      url = "path:/Users/dlipicar/repos/logos-monero-wallet-core-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
      inputs.monero_node_module.follows = "monero_node_module";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      targets = systems ++ [ "x86_64-windows" ];
      forAllTargets = f: nixpkgs.lib.genAttrs targets f;
    in
    {
      packages = forAllTargets (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
