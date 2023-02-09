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
        shell-start-testvm = pkgs.writeShellScriptBin "start-test-vm" ''
         set -e
         export SHARED_DIR=`pwd`
         echo "build test-vm"
         nixos-rebuild build-vm --flake .#testvm
         ./result/bin/run-i2s-test-vm
        '';
        minio-credentials = pkgs.writeText "/etc/minio-credentials" ''
          MINIO_ROOT_USER=test
          MINIO_ROOT_PASSWORD=testme
        '';
        # user is test / testme
        htpasswd-credentials = pkgs.writeText "/etc/htpasswd" ''
          test:G5mNd21eREcPo
        '';
      in let
        # main invoice2storage derivative
        invoice2storage = naersk-lib.buildPackage {
            src = ./.;
            buildInputs =  with pkgs; [cargo rustc] ++ deps;
            nativeBuildInputs = nativeDeps;
        };
      in let
        # function creates a nixosSystem with extra_packages installed
        testVM = extra_packages: nixpkgs.lib.nixosSystem {
            system = system;
            modules = [
              "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
              ({ pkgs, ... }: {

                # boot.isContainer = true;
                documentation.nixos.enable = false;
                # Let 'nixos-version --json' know about the Git revision
                # of this flake.
                system.configurationRevision = nixpkgs.lib.mkIf (self ? rev) self.rev;

                environment.systemPackages = [
                  pkgs.cargo
                  pkgs.rustc
                  pkgs.libressl
                ] ++ extra_packages;
                # Network configuration.
                networking = {
                  hostName = "i2s-test";
                  useDHCP = true;
                  firewall.enable = false;
                };

                services.dovecot2 = {
                  enable = true;
                  mailLocation = "maildir:~/mail";
                  sslServerKey = "/etc/ssl/certs/key.pem";
                  sslServerCert = "/etc/ssl/certs/cert.pem";
                };

                services.postfix = {
                  enable = true;
                };

                services.openssh.enable = true;

                services.minio = {
                  enable = true;
                  dataDir = ["/home/test/files"];
                  rootCredentialsFile = minio-credentials;
                };

                services.webdav-server-rs = {
                  enable = true;
                  user = "test";
                  # debug = true;
                  settings = {
                    server.listen = [ "0.0.0.0:4918" "[::]:4918" ];
                    accounts = {
                      auth-type = "htpasswd.default";
                      acct-type = "unix";
                    };
                    htpasswd.default = {
                      htpasswd = htpasswd-credentials;
                    };
                    location = [
                      {
                        route = [ "/*path" ];
                        directory = "/home/test/webdav";
                        handler = "filesystem";
                        methods = [ "webdav-rw" ];
                        autoindex = true;
                        auth = "write";
                      }
                    ];
                  };
                };

                users.users.test = {
                  password = "testme";
                  group = "test";
                  uid = 1000;
                  extraGroups = [ "wheel" ];
                  isNormalUser = true;
                };
                users.groups.test = {};
                system.stateVersion = "22.11";

                # print ip address on system start
                systemd.services.showIp = {
                  enable = true;
                  script = "ip addr show";
                  after = ["basic.target"];
                  unitConfig = {
                    StandardOutput = "journal+console";
                  };
                };
                services.getty.autologinUser = "test";

                systemd.services."create-ssl-cert" = {
                  description = "Create a certificate for ssl";

                  script = ''
                    ${pkgs.libressl}/bin/openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes -subj '/CN=localhost'
                    chmod 644 cert.pem
                    chmod 640 key.pem
                    chown ssl.ssl key.pem cert.pem
                  '';

                  wantedBy = [ "multi-user.target" "nginx.service" "dovecot2.service"];

                  unitConfig = {
                    Before = [ "multi-user.target" "nginx.service" ]  ;
                    ConditionPathExists = "!/etc/ssl/certs/cert.pem";
                  };

                  serviceConfig = {
                    User = "root";
                    Type = "oneshot";
                    WorkingDirectory = "/etc/ssl/certs";
                    RemainAfterExit = true;
                  };
                };
                # services.greetd = {
                #   enable = true;
                #   settings = {
                #     default_session = {
                #       command = "${pkgs.greetd.greetd}/bin/agreety --cmd 'ip a s; $SHELL'";
                #     };
                #   };
                # };
              })
            ];
            specialArgs = { inherit self; };

          };
      in
      {
        defaultPackage = invoice2storage;

        defaultApp = utils.lib.mkApp {
          drv = self.defaultPackage."${system}";
        };

        devShell = with pkgs; mkShell {
          buildInputs = deps;
          nativeBuildInputs = [ pre-commit rustup nixfmt cargo-watch shell-test-server shell-test-server-stop shell-start-testvm] ++ nativeDeps ++ testDeps;
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          RUSTC_PATH = "${sccache}/bin/sccache";
        };

        packages.nixosConfigurations."testvm" = (testVM [invoice2storage]);
        packages.nixosConfigurations."buildvm" = (testVM []);

      });
}
