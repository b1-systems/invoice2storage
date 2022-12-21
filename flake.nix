{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-22.11";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in let
        deps = with pkgs; [ ];
        nativeDeps = with pkgs; [pkg-config sccache];
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          src = ./.;
          buildInputs =  with pkgs; [cargo rustc] + deps;
          nativeBuildInputs = nativeDeps;
        };

        defaultApp = utils.lib.mkApp {
          drv = self.defaultPackage."${system}";
        };

        devShell = with pkgs; mkShell {
          buildInputs = [ pre-commit rustup nixfmt cargo-watch sccache ] ++ nativeDeps ++ deps;
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          RUSTC_PATH = "${sccache}/bin/sccache";
        };
      });
}
