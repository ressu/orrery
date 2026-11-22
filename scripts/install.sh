#!/bin/sh
# Installs orrery from a GitHub release into ~/orrery, and sets up its service. It can import
# Moonraker's data and switch the printer from Moonraker to orrery in the same run
# (docs/design/2026-11-22-installer.md). Each release has a copy of this script as its
# install.sh, which installs that release:
#
#   curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh -s -- --import --migrate
#
# Run it as the printer's user: orrery runs as the user who installs it. It asks for sudo only
# for setup, and to stop Moonraker while it copies Moonraker's database.
#
# ORRERY_DOWNLOAD_URL, for tests, names a folder (such as file:///tmp/dist) to download the
# archive and SHA256SUMS from, in place of the release.
set -eu

# The release workflow writes the repository and the release into its copy.
default_repo="ressu/orrery"
default_version="latest"

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Installs orrery into ~/orrery and sets up its service. Run it as the printer's user.

  --import, --no-import     Import Moonraker's data, or don't. Without either, it asks.
  --migrate, --no-migrate   Switch from Moonraker to orrery now, or don't. Without either,
                            it asks.
  --no-alias-moonraker      Leave moonraker.service to Moonraker, as when you try orrery
                            beside it.
  --version <vX.Y.Z>        Install this release. The default is this script's release.
  -d, --datapath <folder>   The printer's data folder. The default is ~/printer_data.
  --no-setup                Install the programs only, without the service.
  -h, --help                Show this text.
EOF
}

say() { printf '%s\n' "$*"; }
fail() {
  printf '%s\n' "$@" >&2
  exit 1
}

import=""
migrate=""
alias_flag=""
version="$default_version"
datapath=""
setup=yes
while [ $# -gt 0 ]; do
  case $1 in
    --import) import=yes ;;
    --no-import) import=no ;;
    --migrate) migrate=yes ;;
    --no-migrate) migrate=no ;;
    --no-alias-moonraker) alias_flag="--no-alias-moonraker" ;;
    --version)
      [ $# -ge 2 ] || fail "--version needs a release, such as --version v0.2.0."
      version=$2
      shift
      ;;
    -d | --datapath)
      [ $# -ge 2 ] || fail "$1 needs the printer's data folder, such as $1 ~/printer_data."
      datapath=$2
      shift
      ;;
    --no-setup) setup=no ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      fail "" "The installer doesn't know the option $1."
      ;;
  esac
  shift
done

# A question on the terminal: the facts are printed before it. "$2" is the default, y or n.
# Without a terminal, as in a script, the answer is no.
have_tty() { (: </dev/tty) 2>/dev/null; }
ask() {
  if ! have_tty; then
    return 1
  fi
  if [ "$2" = y ]; then choices="[Y/n]"; else choices="[y/N]"; fi
  printf '%s %s ' "$1" "$choices" >/dev/tty
  read -r answer </dev/tty || answer=""
  case $answer in
    [Yy]*) return 0 ;;
    [Nn]*) return 1 ;;
    *) [ "$2" = y ] ;;
  esac
}

# 1. Checks and questions, before anything changes.
if [ "$(id -u)" -eq 0 ]; then
  fail "Run the installer as the printer's user, not as root." \
    "orrery runs as the user who installs it, and the installer asks for sudo when it needs it."
fi
case $(uname -m) in
  aarch64 | arm64) cpu=aarch64 ;;
  armv7l | armv8l) cpu=armv7 ;;
  x86_64 | amd64) cpu=x86_64 ;;
  *) fail "orrery has no build for this CPU, $(uname -m). Its releases are for aarch64, armv7, and x86_64." ;;
esac
for tool in curl tar sha256sum systemctl; do
  command -v "$tool" >/dev/null 2>&1 || fail "The installer needs $tool, and this system doesn't have it."
done
dir="$HOME/orrery"
if [ -e "$dir/orrery" ]; then
  fail "orrery is already installed in $dir. To update it, run:" "  $dir/orrery self-update"
fi

data=${datapath:-$HOME/printer_data}
# orrery's options for the printer's data folder, when it isn't the default.
set --
if [ -n "$datapath" ]; then
  set -- -d "$datapath"
fi

db="$data/database/moonraker-sql.db"
if [ "$import" != no ] && [ ! -f "$db" ]; then
  if [ -e "$data/database/data.mdb" ]; then
    say "This Moonraker keeps its data in $data/database/data.mdb, as Moonraker did before v0.8."
    say "orrery can't import data from that version. To bring the data along, update Moonraker,"
    say "then run the installer again."
    ask "Install orrery without Moonraker's data?" n || fail "Nothing has changed."
    import=no
  elif [ "$import" = yes ]; then
    say "There's no Moonraker database at $db, so there's nothing to import."
    say "If the printer's data folder is elsewhere, name it with -d."
    ask "Install orrery without importing?" n || fail "Nothing has changed."
    import=no
  else
    import=no
  fi
fi
if [ -z "$import" ]; then
  say "orrery can copy Moonraker's print history, job queue, and settings, so they stay."
  say "Moonraker stops while orrery copies its database."
  if ask "Import Moonraker's data into orrery?" y; then import=yes; else import=no; fi
fi

if systemctl cat moonraker.service >/dev/null 2>&1; then moonraker=yes; else moonraker=no; fi
if [ "$setup" = no ]; then
  migrate=no
elif [ -z "$migrate" ] && [ "$moonraker" = yes ]; then
  say "orrery can take over from Moonraker now: setup stops and disables Moonraker, and starts"
  say "orrery on Moonraker's port. Fluidd, Mainsail, and KlipperScreen then connect to orrery."
  if ask "Switch from Moonraker to orrery now?" y; then migrate=yes; else migrate=no; fi
fi

# 2. Download and check the release.
case $version in
  latest) release="latest/download" ;;
  *) release="download/$version" ;;
esac
base=${ORRERY_DOWNLOAD_URL:-https://github.com/$default_repo/releases/$release}
archive="orrery-$cpu.tar.gz"
# A folder the installer makes goes again when the install fails: rmdir removes it only when
# it's empty.
if [ -d "$dir" ]; then made_dir=no; else made_dir=yes; fi
mkdir -p "$dir"
work="$dir/.install"
rm -rf "$work"
mkdir "$work"
trap 'rm -rf "$work"; if [ "$made_dir" = yes ]; then rmdir "$dir" 2>/dev/null || true; fi' EXIT

say "Downloading $archive..."
curl -fsSL -o "$work/$archive" "$base/$archive" || fail "The installer can't download $base/$archive."
curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" || fail "The installer can't download $base/SHA256SUMS."
grep "  $archive\$" "$work/SHA256SUMS" >"$work/expected" || fail "SHA256SUMS has no checksum for $archive."
(cd "$work" && sha256sum --check --status expected) ||
  fail "$archive doesn't match its checksum, so the installer stopped. Nothing is installed."
mkdir "$work/unpacked"
tar -xzf "$work/$archive" --no-same-owner -C "$work/unpacked"
for file in "$work"/unpacked/*; do
  mv -f "$file" "$dir/"
done
say "Installed $("$dir/orrery" --version) in $dir."

# 3. Preflight: orrery runs on this CPU, and reads the printer's config.
"$dir/orrery" "$@" preflight || fail "orrery's preflight check failed, so the installer stopped."

# Stops when the printer prints, before something stops Moonraker. A Moonraker that runs but
# doesn't answer can't say, so the installer asks.
check_idle() {
  if ! reply=$(curl -fsS -m 5 "http://127.0.0.1:7125/printer/objects/query?print_stats" 2>/dev/null); then
    if systemctl is-active --quiet moonraker.service; then
      say "Moonraker runs, but doesn't say whether the printer is printing."
      ask "Stop Moonraker anyway?" n || fail "Nothing more has changed. orrery is installed in $dir."
    fi
    return 0
  fi
  state=$(printf '%s' "$reply" | sed -n 's/.*"state": *"\([a-z]*\)".*/\1/p')
  case $state in
    printing | paused) fail "The printer is $state, so the installer stopped before stopping Moonraker." \
      "orrery is installed in $dir. When the print ends, see the README for the rest." ;;
  esac
}

# 4. Import.
imported=no
if [ "$import" = yes ]; then
  check_idle
  say "The installer uses sudo to stop Moonraker while it copies Moonraker's database."
  sudo systemctl stop moonraker.service
  cp "$db" "$work/moonraker-sql.db"
  status=0
  "$dir/orrery-import" "$@" "$work/moonraker-sql.db" || status=$?
  if [ "$migrate" != yes ]; then
    say "The installer uses sudo to start Moonraker again."
    sudo systemctl start moonraker.service
  fi
  [ "$status" -eq 0 ] || fail "The import failed. orrery is installed in $dir, without the service."
  imported=yes
fi

# 5. Setup.
if [ "$setup" = yes ]; then
  set -- "$@" setup
  if [ "$migrate" = yes ]; then
    check_idle
    set -- "$@" --migrate
  elif [ "$moonraker" = no ]; then
    set -- "$@" --start
  fi
  if [ -n "$alias_flag" ]; then
    set -- "$@" "$alias_flag"
  fi
  say "The installer uses sudo to set up orrery's service."
  sudo "$dir/orrery" "$@"
fi

# 6. What runs now, and what's next.
say ""
if [ "$setup" = no ]; then
  say "orrery is installed, without its service. To set it up, run:"
  say "  sudo $dir/orrery setup"
elif [ "$migrate" = yes ] || [ "$moonraker" = no ]; then
  say "orrery runs. Fluidd, Mainsail, and KlipperScreen connect to it as they did to Moonraker."
else
  if [ "$imported" = no ] && [ -f "$db" ]; then
    say "To bring Moonraker's data along first, follow \"Moving from Moonraker\" in orrery's README:"
    say "  https://github.com/$default_repo#moving-from-moonraker"
  fi
  say "orrery is set up, and Moonraker still runs. To switch from Moonraker to orrery, run:"
  say "  sudo $dir/orrery setup --migrate"
fi
say "To update orrery, run:"
say "  $dir/orrery self-update"
if [ "$moonraker" = yes ] && [ "$migrate" = yes ]; then
  say "To go back to Moonraker, run:"
  say "  sudo $dir/orrery setup --remove"
fi
