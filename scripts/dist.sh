#!/usr/bin/env bash
# Build a release archive of orrery for one CPU, and add its checksum to dist/SHA256SUMS.
#
# Usage:
#   scripts/dist.sh <arch>
#
# Arguments:
#   arch  aarch64, armv7, or x86_64
#
# The archive is dist/orrery-<arch>.tar.gz: a fixed name, as an update entry's asset_name is.
# It holds orrery, orrery-gpio, orrery-import, orrery-spoolman, orrery-updater, LICENSE, and README.md,
# with no folder around them, so it unpacks straight into ~/orrery. The binaries are release
# builds (ORRERY_RELEASE=1). In CI, GITHUB_REPOSITORY names the repository they update from,
# and the archive gets a release_info.json that names it, the version, and the archive, as an
# executable update entry needs. Without it, there's no release_info.json, and the programs
# don't update themselves.
#
# The archive is the same for the same commit: its files are owned by root and dated with
# the commit's time. (The nix shell's SOURCE_DATE_EPOCH is a fixed 1980 date, so it's not used.)
#
# Examples:
#   scripts/dist.sh aarch64
#   GITHUB_REPOSITORY=someone/orrery scripts/dist.sh armv7

set -euo pipefail

usage() {
  sed -n '2,/^$/p' "$0" | sed -e '/^$/d' -e 's/^# \{0,1\}//'
  exit "$1"
}

if [ $# -ne 1 ]; then
  usage 1
fi
case "$1" in
  aarch64) target=aarch64-unknown-linux-musl ;;
  armv7) target=armv7-unknown-linux-musleabihf ;;
  x86_64) target=x86_64-unknown-linux-musl ;;
  -h | --help) usage 0 ;;
  *)
    echo "orrery has no release for $1. Use aarch64, armv7, or x86_64." >&2
    exit 1
    ;;
esac
arch=$1
cd "$(dirname "$0")/.."

# The version is Cargo's, with a v: 0.2.0 is v0.2.0.
cargo_version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
version="v$cargo_version"

echo "==> Building orrery $version for $arch ($target)"
ORRERY_RELEASE=1 cargo build --quiet --release --locked --target "$target" \
  -p orrery -p orrery-gpio -p orrery-spoolman -p orrery-updater --bins

stage="target/dist/$arch"
rm -rf "$stage"
mkdir -p "$stage" dist
files=(orrery orrery-gpio orrery-import orrery-spoolman orrery-updater)
for name in "${files[@]}"; do
  install -m 755 "target/$target/release/$name" "$stage/$name"
done
install -m 644 LICENSE README.md "$stage/"
files+=(LICENSE README.md)

archive="orrery-$arch.tar.gz"
if [ -n "${GITHUB_REPOSITORY:-}" ]; then
  printf '{"project_name": "%s", "project_owner": "%s", "version": "%s", "asset_name": "%s"}\n' \
    "${GITHUB_REPOSITORY#*/}" "${GITHUB_REPOSITORY%%/*}" "$version" "$archive" > "$stage/release_info.json"
  chmod 644 "$stage/release_info.json"
  files+=(release_info.json)
else
  echo "==> GITHUB_REPOSITORY isn't set, so the archive has no release_info.json"
fi
epoch=$(git log -1 --format=%ct)
tar --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
  -C "$stage" -cf - "${files[@]}" | gzip -9n > "dist/$archive"

# One line per archive: replace this archive's line, keep the others.
sums=dist/SHA256SUMS
touch "$sums"
line=$(cd dist && sha256sum "$archive")
{ grep -v "  $archive\$" "$sums" || true; echo "$line"; } | sort -k2 > "$sums.new"
mv "$sums.new" "$sums"
echo "==> dist/$archive ($(du -h "dist/$archive" | cut -f1)), checksum in $sums"
