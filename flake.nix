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
        deps = with pkgs; [ openssl ];
        nativeDeps = with pkgs; [pkg-config sccache];
        testDeps = with pkgs; [dave minio mkcert dovecot];

        shell-test-server = pkgs.writeShellScriptBin "start-test-server" ''
    set -xe
    WORKDIR="''${WORKDIR:-/tmp/invoice2storage}"
    mkdir -p $WORKDIR
    if [ ! -e "$WORKDIR/.ssl/server.key" ]; then
      mkdir -p $WORKDIR/.ssl
      # openssl req  -nodes -new -x509  -keyout $WORKDIR/.ssl/server.key -out $WORKDIR/.ssl/server.crt -subj '/CN=localhost'
      mkcert localhost -cert-file $WORKDIR/.ssl/server.crt -key-file $WORKDIR/.ssl/server.key
    fi
    truncate -s 0 $WORKDIR/.pids
    ${pkgs.dufs}/bin/dufs -A --port 5443 --tls-cert $WORKDIR/.ssl/server.crt --tls-key $WORKDIR/.ssl/server.key $WORKDIR &
    echo $! >> $WORKDIR/.pids
    ${pkgs.minio}/bin/minio server /tmp/invoice2storage &
    echo $! >> $WORKDIR/.pids
  '';
        shell-test-server-stop = pkgs.writeShellScriptBin "stop-test-server" ''
    WORKDIR="''${WORKDIR:-/tmp/invoice2storage}"
    cat $WORKDIR/.pids | xargs kill -9
  '';
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
          buildInputs = deps;
          nativeBuildInputs = [ pre-commit rustup nixfmt cargo-watch shell-test-server shell-test-server-stop ] ++ nativeDeps ++ testDeps;
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          RUSTC_PATH = "${sccache}/bin/sccache";
        };
      });
}
