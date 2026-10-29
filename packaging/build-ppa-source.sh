#!/usr/bin/env bash
# Build a source package for a Launchpad PPA upload.
# Requires: ./packaging/prepare-ppa.sh (creates vendor/ and .cargo/config.toml).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

series="${1:-noble}"
if [[ ! -d vendor ]] || [[ ! -f .cargo/config.toml ]]; then
  echo "Run ./packaging/prepare-ppa.sh first (vendor/ missing)." >&2
  exit 1
fi

if ! command -v dpkg-parsechangelog >/dev/null || ! command -v dpkg-buildpackage >/dev/null; then
  echo "Install packaging tools: sudo apt install debhelper devscripts dpkg-dev" >&2
  exit 1
fi

version="$(dpkg-parsechangelog -S Version)"
source_name="$(dpkg-parsechangelog -S Source)"
upstream="${version%-*}"
parent="$(cd .. && pwd)"
orig="${parent}/${source_name}_${upstream}.orig.tar.xz"
staged="${parent}/${source_name}-${upstream}"

# Rewrite the distribution for this upload; restore changelog on exit.
changelog_backup="$(mktemp)"
cp debian/changelog "$changelog_backup"
cleanup() {
  cp "$changelog_backup" debian/changelog
  rm -f "$changelog_backup"
  rm -rf "$staged"
}
trap cleanup EXIT

tmp_cl="$(mktemp)"
awk -v series="$series" '
  NR == 1 { sub(/\)[[:space:]]+[a-z0-9]+;/, ") " series ";") }
  { print }
' debian/changelog >"$tmp_cl"
mv "$tmp_cl" debian/changelog

echo "Creating ${orig} (includes vendor/)..."
rm -rf "$staged"
mkdir -p "$staged"
# Ship the sources Launchpad needs; skip local build artifacts.
tar \
  --exclude='./target' \
  --exclude='./debian/cargo_home' \
  --exclude='./.git' \
  --exclude='./.gitignore' \
  -C "$root" \
  -cf - \
  . \
| tar -C "$staged" -xf -

# Orig tarball must not contain debian/ (quilt): strip it for the orig, keep working tree.
rm -rf "$staged/debian"
tar -C "$parent" -cJf "$orig" "${source_name}-${upstream}"

# Put debian/ back into the staged tree for the packaging build.
cp -a "$root/debian" "$staged/debian"

sign_args=(-us -uc)
if [[ "${DPUT_SIGN:-}" == "1" ]] || [[ -n "${DEBSIGN_KEYID:-}" ]]; then
  sign_args=()
  if [[ -n "${DEBSIGN_KEYID:-}" ]]; then
    sign_args+=(-k"$DEBSIGN_KEYID")
  fi
fi

echo "Building source package ${source_name} ${version} for ${series}..."
(
  cd "$staged"
  # Link or copy vendor already in staged from the tar extract above.
  dpkg-buildpackage -S -sa -d "${sign_args[@]}"
)

changes="${parent}/${source_name}_${version}_source.changes"
echo
echo "Source package:"
echo "  ${changes}"
echo "  ${orig}"
echo
echo "Sign (if built with -us -uc) and upload:"
echo "  debsign ${changes}"
echo "  dput ppa:deppo/deppo ${changes}"
