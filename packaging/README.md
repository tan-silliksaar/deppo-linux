# Launchpad PPA packaging

Launchpad builds from a **signed source package** with **no network**. Crates must be vendored into `vendor/` before upload.

## One-time Launchpad setup

1. Launchpad user [~deppo](https://launchpad.net/~deppo) and project [deppo](https://launchpad.net/deppo).
2. Upload your GPG key to Launchpad and sign the Ubuntu Code of Conduct.
3. Create the PPA `~deppo/ubuntu/deppo` (shown to users as `ppa:deppo/deppo`) if it does not exist yet.
4. Install build tools locally:

```bash
sudo apt install debhelper devscripts dput cargo rustc pkg-config libfuse3-dev libssl-dev
```

## Prepare and upload a release

From the repository root (clean working tree, matching `Cargo.toml` version):

```bash
./packaging/prepare-ppa.sh
./packaging/build-ppa-source.sh noble
dput ppa:deppo/deppo ../deppo_1.0.5-1_source.changes
```

`prepare-ppa.sh` runs `cargo vendor`, writes `.cargo/config.toml` for offline builds, and checks that `Cargo.lock` is present.

`build-ppa-source.sh` runs `dpkg-buildpackage -S` (source-only). Launchpad compiles the binary in its chroot.

Repeat `build-ppa-source.sh` with another series name (`jammy`, `plucky`, …) after editing `debian/changelog` for that series, or pass the series as the first argument (the script rewrites the changelog distribution for that run).

## Rust version

This crate uses Rust **edition 2024**, so the build chroot needs **rustc/cargo ≥ 1.85**. If an Ubuntu series ships an older toolchain, either:

- publish only for newer series, or
- add a Launchpad Rust toolchain PPA as a dependency of your PPA and adjust `Build-Depends` in `debian/control`.

## What users install

```bash
sudo add-apt-repository ppa:deppo/deppo
sudo apt update
sudo apt install deppo
```

`fuse3` and `libfuse3` are pulled in automatically via package dependencies.

## Local binary package (optional)

For a quick local `.deb` (not for Launchpad):

```bash
./packaging/prepare-ppa.sh
dpkg-buildpackage -b -us -uc
```

## Files not committed

`vendor/` and `.cargo/config.toml` are generated and gitignored. They must be present in the tree when you build the source package so they are included in the upload.
