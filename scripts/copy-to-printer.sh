#!/usr/bin/env bash
# Build orrery, orrery-import, orrery-spoolman, and orrery-update for a printer's CPU, and copy
# them there.
#
# Usage:
#   scripts/copy-to-printer.sh <ssh host>
#   scripts/copy-to-printer.sh --disconnect <ssh host>
#
# Options:
#   --disconnect  close the SSH connection. Without it, the connection closes 10 minutes after the last copy.
#
# The binaries go to ~/orrery-test/bin on the printer. Each is copied only when it changed.
# The script doesn't start orrery, and it doesn't touch Moonraker, Klipper, or printer_data.
# Remove everything with: ssh <ssh host> 'rm -rf ~/orrery-test'
#
# Examples:
#   scripts/copy-to-printer.sh pi@printer-1
#   scripts/copy-to-printer.sh --disconnect pi@printer-1

set -euo pipefail

usage() {
  sed -n '2,/^$/p' "$0" | sed -e '/^$/d' -e 's/^# \{0,1\}//'
  exit "$1"
}

disconnect=0
while [ $# -gt 0 ]; do
  case "$1" in
    --disconnect) disconnect=1 ;;
    -h | --help) usage 0 ;;
    -*)
      echo "Unknown option: $1" >&2
      usage 1
      ;;
    *) break ;;
  esac
  shift
done
if [ $# -ne 1 ]; then
  usage 1
fi
host=$1

# The "~" is expanded by the shell on the printer, not here.
# shellcheck disable=SC2088
test_dir='~/orrery-test'

# One SSH connection, kept open for 10 minutes after the last use, so the password is asked only once.
# The socket is in /tmp because socket paths must be short; $TMPDIR can be long (for example in Nix shells).
# %C is a hash of the user, host, and port.
control_dir="/tmp/orrery-ssh-$(id -u)"
mkdir -p "$control_dir"
chmod 700 "$control_dir"
ssh_options=(-o ControlMaster=auto -o ControlPath="$control_dir/%C" -o ControlPersist=10m)
ssh() { command ssh "${ssh_options[@]}" "$@"; }
scp() { command scp "${ssh_options[@]}" "$@"; }

if [ "$disconnect" = 1 ]; then
  ssh -O exit "$host" 2>/dev/null || true
  echo "Closed the SSH connection to $host."
  exit 0
fi

# One call gets the CPU and the checksums of the binaries that are on the printer now.
binaries=(orrery orrery-import orrery-spoolman orrery-update)
# shellcheck disable=SC2029 # The variables are meant to expand here, on this computer.
info=$(ssh "$host" "mkdir -p $test_dir/bin && uname -m && cd $test_dir/bin && { sha256sum ${binaries[*]} 2>/dev/null || true; }")
arch=$(sed -n 1p <<<"$info")
case "$arch" in
  aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
  armv7*) target=armv7-unknown-linux-musleabihf ;;
  x86_64) target=x86_64-unknown-linux-musl ;;
  *)
    echo "orrery can't be built for the CPU of $host: $arch" >&2
    echo "The supported CPUs are aarch64, armv7, and x86_64." >&2
    exit 1
    ;;
esac

# cargo builds again only if the source changed.
echo "==> Building for $arch ($target)"
cargo build --quiet --release -p orrery -p orrery-spoolman -p orrery-update --bins --target "$target"

for name in "${binaries[@]}"; do
  binary="target/$target/release/$name"
  remote_sum=$(awk -v n="$name" '$2 == n { print $1 }' <<<"$info")
  if [ "$(sha256sum "$binary" | sed 's/ .*//')" = "$remote_sum" ]; then
    echo "==> $name on $host is up to date"
  else
    echo "==> Copying $name to $host:$test_dir/bin/"
    # Copy to a temporary name, then rename, so a running orrery isn't overwritten mid-copy.
    scp -q "$binary" "$host:orrery-test/bin/$name.new"
    ssh "$host" "chmod +x $test_dir/bin/$name.new && mv $test_dir/bin/$name.new $test_dir/bin/$name"
  fi
done
echo "    Run it with: ssh $host '$test_dir/bin/orrery'"
