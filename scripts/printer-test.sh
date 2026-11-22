#!/usr/bin/env bash
# Run orrery on a printer as a temporary systemd unit, orrery-test, from ~/orrery-test/bin.
#
# Usage:
#   scripts/printer-test.sh start <ssh host> [orrery options]
#   scripts/printer-test.sh stop <ssh host>
#   scripts/printer-test.sh status <ssh host>
#   scripts/printer-test.sh log <ssh host>
#
# Commands:
#   start   stop Moonraker and orrery.service if they run, then start orrery-test. It runs as
#           your user with the orrery-admin and moonraker-admin groups, those that exist, which
#           the polkit rules of orrery setup and Moonraker's installer trust to manage services.
#           orrery-test replaces an orrery-test that's already running.
#   stop    stop orrery-test, and start again what start stopped.
#   status  show orrery-test's state.
#   log     follow orrery-test's log.
#
# systemd-run makes the unit; no unit file is written, and the unit is gone once it stops.
# ~/orrery-test/orrery.toml is used when it exists. When layerlock is installed, start refuses
# while the printer prints or is paused. It also refuses while another orrery runs outside the
# unit, such as in tmux.
# start and stop use sudo, which may ask for your password.
#
# Examples:
#   scripts/printer-test.sh start pi@printer-1
#   scripts/printer-test.sh start pi@printer-1 --verbose
#   scripts/printer-test.sh stop pi@printer-1

set -euo pipefail

usage() {
  sed -n '2,/^$/p' "$0" | sed -e '/^$/d' -e 's/^# \{0,1\}//'
  exit "$1"
}

if [ $# -lt 2 ]; then
  case "${1:-}" in
    -h | --help) usage 0 ;;
    *) usage 1 ;;
  esac
fi
command=$1
host=$2
shift 2
case "$command" in
  start | stop | status | log) ;;
  *)
    echo "Unknown command: $command" >&2
    usage 1
    ;;
esac

# The same SSH connection as copy-to-printer.sh and `just connect-printer`.
control_dir="/tmp/orrery-ssh-$(id -u)"
mkdir -p "$control_dir"
chmod 700 "$control_dir"
ssh_options=(-o ControlMaster=auto -o ControlPath="$control_dir/%C" -o ControlPersist=10m)

# This part runs on the printer.
# shellcheck disable=SC2016 # The variables expand on the printer.
remote='
set -euo pipefail
unit=orrery-test
dir=$HOME/orrery-test
# The services start stopped, so stop can start them again.
stopped=$dir/stopped-services

case "$1" in
start)
  shift
  # layerlock exits 1 while the printer prints and 2 while it'"'"'s paused. It exits 255 when it
  # can'"'"'t ask, such as when no Moonraker or orrery is running; then there'"'"'s nothing to stop.
  if command -v layerlock >/dev/null; then
    state=0
    layerlock 2>/dev/null || state=$?
    case "$state" in
      1 | 2)
        echo "The printer is printing or paused (layerlock), so orrery-test isn'"'"'t started." >&2
        exit 1
        ;;
      255) echo "layerlock can'"'"'t tell whether the printer is busy; no API server answers." ;;
    esac
  fi
  if [ ! -x "$dir/bin/orrery" ]; then
    echo "There'"'"'s no $dir/bin/orrery. Copy it first with: just copy-to-printer <ssh host>" >&2
    exit 1
  fi
  sudo systemctl stop "$unit" 2>/dev/null || true
  sudo systemctl reset-failed "$unit" 2>/dev/null || true
  if pgrep -x orrery >/dev/null; then
    echo "Another orrery is running (process $(pgrep -x orrery | head -1)), outside orrery-test." >&2
    echo "Stop it first, for example with Ctrl-C in its tmux pane." >&2
    exit 1
  fi
  for service in moonraker orrery; do
    if systemctl is-active --quiet "$service"; then
      sudo systemctl stop "$service"
      echo "$service" >>"$stopped"
      echo "Stopped $service; stop starts it again."
    fi
  done
  args=()
  if [ -f "$dir/orrery.toml" ]; then
    args+=(--orrery-config "$dir/orrery.toml")
  fi
  # orrery-admin comes from sudo orrery setup, moonraker-admin from Moonraker'"'"'s installer.
  # orrery-test gets each one that exists, so either polkit rule lets it manage services.
  admin_groups=""
  for group in orrery-admin moonraker-admin; do
    if getent group "$group" >/dev/null; then
      admin_groups="$admin_groups $group"
    fi
  done
  groups=()
  if [ -n "$admin_groups" ]; then
    groups=(-p "SupplementaryGroups=${admin_groups# }")
  else
    echo "There'"'"'s no orrery-admin or moonraker-admin group, so orrery-test can'"'"'t manage services. Run: sudo orrery setup" >&2
  fi
  sudo systemd-run --quiet --unit="$unit" --uid="$(id -u)" --gid="$(id -g)" "${groups[@]}" \
    -p WorkingDirectory="$dir/bin" -p KillMode=mixed \
    "$dir/bin/orrery" "${args[@]}" "$@"
  sleep 2
  if systemctl is-active --quiet "$unit"; then
    echo "orrery-test is running. Follow its log with: scripts/printer-test.sh log <ssh host>"
  else
    echo "orrery-test stopped right away. Its log:" >&2
    journalctl -u "$unit" -n 20 --no-pager >&2
    exit 1
  fi
  ;;
stop)
  if systemctl is-active --quiet "$unit"; then
    sudo systemctl stop "$unit"
    echo "Stopped orrery-test."
  else
    echo "orrery-test isn'"'"'t running."
    sudo systemctl reset-failed "$unit" 2>/dev/null || true
  fi
  if [ -f "$stopped" ]; then
    for service in $(sort -u "$stopped"); do
      sudo systemctl start "$service"
      echo "Started $service again."
    done
    rm "$stopped"
  fi
  ;;
status)
  systemctl status "$unit" --no-pager || true
  ;;
log)
  journalctl -u "$unit" -f -n 50
  ;;
esac
'

# -t gives sudo a terminal to ask for a password on.
exec ssh -t "${ssh_options[@]}" "$host" "bash -c $(printf %q "$remote") orrery-test $(printf '%q ' "$command" "$@")"
