# Launchpad PPA packaging

Launchpad builds from a **signed source package** with **no network**. Crates must be vendored into `vendor/` before upload.

## Supported Ubuntu series

Minimum: **Ubuntu 22.04 LTS (jammy)** — distro `rustc`/`cargo` 1.75+.

Also build for **noble** (24.04) and newer LTS/interim releases the same way. Each series gets its own source upload (or a recipe build per series); `./packaging/build-ppa-source.sh SERIES` rewrites the changelog distribution for that upload.

Do not target releases older than jammy: their default Rust is too old for this crate.

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
./packaging/build-ppa-source.sh jammy
dput ppa:deppo/deppo ../deppo_1.0.6-2_source.changes
```

Repeat for other series (same upstream, new changelog distribution via the script):

```bash
./packaging/build-ppa-source.sh noble
./packaging/build-ppa-source.sh resolute
```

`prepare-ppa.sh` runs `cargo vendor`, writes `.cargo/config.toml` for offline builds, and strips `Cargo.toml.orig` from `vendor/` (dpkg-source drops `*.orig` when packing, which would break cargo checksums on Launchpad).

## Recipe text (optional)

Launchpad recipes have **no network**, so the Git branch the recipe builds
must already contain a complete `vendor/` tree and a matching
`.cargo/config.toml` (run `./packaging/prepare-ppa.sh` and commit those, or
prefer the `dput` source-package flow above which embeds them in the
`.orig.tar.xz`). A recipe against a branch that only has `debian/` will fail
at `cargo build --offline`.

```text
# git-build-recipe format 0.4 deb-version {debversion}~{revtime}
lp:~deppo/deppo/+git/deppo main
```

Request builds for **jammy**, **noble**, and any newer series you support. Launchpad rewrites the changelog distribution per series when using recipes.

## Regenerating Cargo.lock

Keep `Cargo.lock` at **version = 3** and free of edition-2024 crates
(`wit-bindgen`, etc.). Prefer regenerating with a modern toolchain plus:

```toml
# .cargo/config.toml (local only; prepare-ppa.sh overwrites this with vendor sources)
[resolver]
incompatible-rust-versions = "fallback"
```

Do **not** run an unpinned `cargo update` on a new toolchain without checking
the lock afterward — crates like `tempfile` ≥ 3.15 pull `getrandom` 0.3 →
`wasip2` → `wit-bindgen`, which jammy's cargo 1.75 cannot parse when
vendoring. The pins in `Cargo.toml` (`getrandom`, `ignore`, `globset`,
`tempfile`) are intentional.

## What users install

```bash
sudo add-apt-repository ppa:deppo/deppo
sudo apt update
sudo apt install deppo
```

`fuse3` and the matching `libfuse3` soname are pulled in via package dependencies.

## Local binary package (optional)

```bash
./packaging/prepare-ppa.sh
dpkg-buildpackage -b -us -uc
```

## Files not committed

`vendor/` and `.cargo/config.toml` are generated and gitignored. They must be present in the tree when you build the source package so they are included in the upload.
