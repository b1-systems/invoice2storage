# Invoice2storage

Easy handling for invoices in cooperate environments.

Workflow:

Each user gets a folder accessible for the office. Users can forward their invoices
to a special email address like invoice+bob.allen@example.com

The office has one place where all invoices are collected.

## Operation

This script is used a a email filter to process incoming invoice emails.

1. This script parses emails from stdin or file
2. It determines the user this invoice belongs to
   - if the target email contains a + suffix, the suffix is the user
   - the the from and to domains mach, the sender is the user
3. It tries to  extracts all attachments of certain mime types, defaults to pdf files.
4. It stores the extracted attachments according in the folder specified by template
5. It stores the email in the folder and backend configured

## Installation

### Using cargo

```bash
cargo install invoice2storage
```

### Using nix flake

You can add this repository to your NixOS flake configuration.

## Configuration

All settings can be passed through command line arguments or put into a yaml file.
See `--help` for a full list of options.

```bash
A email processor to extract email attachments and store them on a storage backend. like webdav, directory, s3, ...

Usage: invoice2storage [OPTIONS] [FILE]

Arguments:
  [FILE]  File to extract [default: -]

Options:
      --config-file <CONFIG_FILE>
          Config file to load [default: ~/.config/invoice2storage/config.toml]
      --unknown-user <UNKNOWN_USER>
          user name for unknown user [default: _UNKNOWN]
      --accepted-mimetypes <ACCEPTED_MIMETYPES>
          [default: application/pdf]
  -v, --verbose...
          Increase verbosity
  -q, --quiet
          Silence all output
      --local-path [<LOCAL_PATH>]
          Local path to save extensions to [env: LOCAL_PATH=]
      --http-path [<HTTP_PATH>]
          Store extensions at webdav target [env: HTTP_PATH=]
      --insecure <INSECURE>
          Ignore tls/https errors [possible values: true, false]
      --overwrite-user [<OVERWRITE_USER>]
          Overwrite the detected user with specified
      --stdout <STDOUT>
          Pipe mail to stdout. Useful when used as a pipe filter [possible values: true, false]
      --output-template <OUTPUT_TEMPLATE>
          template for file output path [env: OUTPUT_TEMPLATE=] [default: "{{user | lower}}/{{file_name | escape_filename}}"]
      --maildir-path [<MAILDIR_PATH>]
          Maildir folder to save messages to, instead of imap [env: MAILDIR_PATH=]
      --imap-url [<IMAP_URL>]
          IMAP connection url. imaps://user:password@host [env: IMAP_URL=]
      --mail-template <MAIL_TEMPLATE>
          Mail template folder [env: MAIL_TEMPLATE=] [default: "{{user | lower}}.{% if errors %}new{% else %}done{% endif %}"]
      --success-flags <SUCCESS_FLAGS>
          Mail flags in success case [env: SUCCESS_FLAGS=] [default: ]
      --error-flags <ERROR_FLAGS>
          Mail flags in error cases [env: ERROR_FLAGS=] [default: \Flag]
  -h, --help
          Print help
  -V, --version
          Print version
```

## MTA configuration

Most MTA support `.forward` pipe support which allows you to configure invoice2storage like this:

`~/.forward` contains:

```sh
|/path/to/invoice2storage --arguments....
```

## Development

All dev tools use the [nix](https://nixos.org/) package manager, which can be used on any linux distribution. This allows 100% reproducible and working dev environments.

### Development server

Integration test is done with a NixOS VM that is created with:

```bash
nixos-rebuild build-vm --flake .#testvm

QEMU_OPTS="-netdev bridge,id=hn0,br=intern -device e1000,netdev=hn0" ./result/bin/run-i2s-test-vm
```

The QEMU_OPTS depend on your system, in this example, the VM is attached to local bridge `intern`.

The tests can be run with:
```bash
env RUST_BACKTRACE=1 TARGET=10.0.42.158  /home/poelzi/b1/invoice2storage/scripts/nix-cargo test --package invoice2storage --bin invoice2storage -- --include-ignored
```

The environment variables `TARGET` is the VM address.
