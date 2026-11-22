# Changelog

What changes in each release of orrery. Read it before you update, so you know what to expect.

Fluidd and Mainsail link to the release page on GitHub, which shows the same notes.

## Unreleased

### Fixed

- The `orrery` update entry no longer warns "orrery put v0.1.1 in place, but runs v0.1.2"
  after orrery is installed again over an older install's database. The warning now compares
  the programs in `~/orrery` with the orrery that runs.

## 0.1.2 (2026-11-22)

### New

- A one-line installer: `curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh -s -- --import --migrate`
  installs orrery in `~/orrery`, imports Moonraker's data, and switches the printer to orrery.
  Without `--import` and `--migrate`, it asks. It checks the download's checksum, stops if
  the printer is printing, and stops if orrery is already installed. See the README's
  Installing section.

### Changed

- With Moonraker compatibility on, `sudo orrery setup` makes `moonraker.service` another name
  for orrery's service by default, so services that start Moonraker start orrery.
  `--migrate` now moves Moonraker's own service file aside for that, as `--force` does.
- To keep Moonraker's service file, as when you try orrery beside Moonraker, run setup with
  `--no-alias-moonraker`, or set `service_alias = false` under `[moonraker_compat]` in
  `orrery.toml`. Later runs of setup keep the choice.

### Fixed

- `sudo orrery setup` no longer restarts orrery without `--start`. When orrery runs and its
  service has to restart to take or give up the name `moonraker.service`, setup leaves it as it
  is, and tells you to run it again with `--start`. It also no longer says orrery runs the old
  service file right after restarting it.

## 0.1.1 (2026-11-22)

### New

- `sudo orrery setup --alias-moonraker` makes `moonraker.service` another name for orrery's
  service. Services that start or wait for Moonraker, such as aldis, then get orrery, and no
  longer start Moonraker and stop orrery. If Moonraker's own service file is in the way,
  setup changes nothing unless you add `--force`. `--force` disables Moonraker and moves its
  file aside, and `sudo orrery setup --remove` puts it back.

## 0.1.0 (2026-11-22)

First release.
