# Install

rbtrfs runs on Linux and needs a btrfs filesystem.

## Downloads

Tarballs for x86_64 and aarch64 are on the
[releases page](https://github.com/spion06/restic-btrfs/releases). Each contains the
`rbtrfs` binary, a man page, shell completions, the licences and an example config.
Verify the download with the `.sha256` file next to it, then copy the binary into
your `PATH`:

    tar xzf rbtrfs-v0.1.0-x86_64-linux.tar.gz
    sudo install -m755 rbtrfs-v0.1.0-x86_64-linux/rbtrfs /usr/local/bin/

The binary links against `libbtrfsutil`, which comes with `btrfs-progs` (package
`libbtrfsutil1` on Debian and Ubuntu). The functions rbtrfs uses exist in
every version from btrfs-progs 5.16 (checked against the library's symbol list, not by
running each version). It needs glibc 2.34 or newer, which should cover Debian 12,
Ubuntu 22.04, RHEL 9 and anything more recent. `mount(8)` is only needed if you use
`repository_mount`.

## From source

You need Rust 1.91 or newer, `pkg-config`, libclang and the `libbtrfsutil` headers
from btrfs-progs 6.8 or newer. Debian 13 and Ubuntu 25.10 or newer package those as
`libbtrfsutil-dev`. On older distributions the packaged header is too old for the Rust
bindings. `.github/scripts/install-libbtrfsutil.sh` builds and installs a current
`libbtrfsutil` into `/usr/local`, which is what the release builds do.

    git clone https://github.com/spion06/restic-btrfs
    cd restic-btrfs
    cargo install --path .

## Man page and completions

The binary prints both itself:

    rbtrfs man | sudo tee /usr/local/share/man/man1/rbtrfs.1 >/dev/null
    rbtrfs completions bash | sudo tee /etc/bash_completion.d/rbtrfs >/dev/null

`zsh`, `fish`, `elvish` and `powershell` are also supported.
