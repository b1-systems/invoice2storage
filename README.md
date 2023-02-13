# Invoice2storage

Easy handling for invoices in cooperate environments with a central place to collect invoices.

**Workflow**:

Each user/employee gets a folder accessible for the office. Users can forward their invoices
to a personalized email address like invoice+bob.allen@example.com

Invoice2storage extracts the attached files from the email, determines the user and stores the files in the users directory.

The office has one place where all invoices are collected, sorted by user (can be customized).

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

The suggested architecture is to run invoice2storage on the EMail-server, that stores the Maildir/IMAP folders. Invoice2storage can store the emails in `maildir` or `imap` folders.

### Using cargo

```bash
cargo install invoice2storage
```

### Using nix flake

You can add this repository to your NixOS flake configuration.

## Configuration

All settings can be passed through command line arguments or put into a yaml file.
See `--help` for a full list of options.


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

### Suggested VSCode workspace settings

```json
   "cSpell.customDictionaries": {
      "custom-dictionary-workspace": {
         "name": "custom-dictionary-workspace",
         "path": "${workspaceFolder:invoice2storage}/.cspell/custom-dictionary-workspace.txt",
         "addWords": true,
         "scope": "workspace"
      }
   },
   "rust-analyzer.runnables.command": "${workspaceFolder}/scripts/nix-cargo",
   "cargo.automaticCheck": false,
```
