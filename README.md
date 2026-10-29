# deppo

Linux client for [Deppo](https://www.deppo.eu) storage. The binary is named `deppo`.

Version 1.0.5. Apache License 2.0.

## Status

- Subscriptions, folders, timers, and backups work.
- Config is `~/.config/deppo/config.toml` (passwords encrypted with a local key).
- One background instance per user (`flock` under `$XDG_RUNTIME_DIR`).
- `deppo --ui` opens the local page in a browser.
- `deppo enable-startup` installs a systemd user unit.
- Virtual drives use FUSE 3 at the configured mount path (empty directory).

## Install from a PPA (when published)

```bash
sudo add-apt-repository ppa:deppo/deppo
sudo apt update
sudo apt install deppo
```

`fuse3` is installed automatically as a package dependency. Packaging and upload steps for maintainers are in [packaging/README.md](packaging/README.md).

## Build

Needs `fuse3` and `libfuse3-dev` (and OpenSSL headers for `native-tls`):

```bash
sudo apt install fuse3 libfuse3-dev libssl-dev pkg-config
cargo build --release
./target/release/deppo --help
```

## Examples

```bash
deppo subscription add h1 12345678:password
deppo folder add h1 12345678 ~/Documents
deppo mount set h1 12345678 ~/Deppo
deppo timer set h1 12345678 "6 hours"
deppo backup h1 12345678
deppo --ui
```

`deppo mount set` creates the mount directory when it does not exist. The directory must be empty. While Deppo is not mounted there, the directory is kept non-writable so stray copies cannot fill it.

Removing a folder in the mount (`rmdir`, or any client that issues one directory remove) deletes the whole tree with a single server request. Prefer that over `rm -rf` for large folders; some file managers still walk children themselves before removing the directory.
