# Changelog

What changes in each release of orrery. Read it before you update, so you know what to expect.

Fluidd and Mainsail link to the release page on GitHub, which shows the same notes.

## Unreleased

### Fixed

- Fluidd no longer shows "Spoolman server not available" when it connects while Orrery is
  starting. A Spoolman request waits up to a few seconds for the first connection to
  Spoolman.

## 0.2.0 (2026-11-25)

### New

- Buttons on the printer computer's own GPIO lines, with `[button.<name>]` in `orrery.toml`.
  A button runs actions when it's pressed, released, clicked, or held. A `[button]` from
  `moonraker.conf` works when its template is a single `call_method`; see
  [Buttons](https://github.com/ressu/orrery/blob/main/docs/configuration/buttons.md).
- Actions: named steps, `[action.<name>]` in `orrery.toml`, that buttons, Klipper's macros
  (`action_call_remote_method("run_action", action="…")`), and clients run, each with the role
  it needs. Built in: `shutdown`, `reboot`, `emergency_stop`, `job_queue_pause`,
  `job_queue_start`, and `spoolman.set_active_spool`; see
  [Actions](https://github.com/ressu/orrery/blob/main/docs/configuration/actions.md).
- The update manager has a new entry type, `archive`, for programs released as an archive of
  files, such as aldis. It installs every file of the release, removes the files a new release
  drops, and keeps the old ones for a rollback. Set it up in `orrery.toml`; see
  [the update manager](https://github.com/ressu/orrery/blob/main/docs/configuration/update-manager.md#archive-entries).
- The Services page lists the service of each update manager entry, so it doesn't need a line
  in `moonraker.asvc`. `managed_services` can name a service that runs with
  `systemctl --user`, as `user/<name>`.
- Updating Orrery installs every file of the release, so a release can bring a new program,
  such as `orrery-gpio`, and removes the files a release drops. A rollback removes the files
  the older version doesn't have.
- An update manager entry can install what it describes: with `install = true`, or a command
  to run after the release is in place, the first update installs it, so adding the entry and
  pressing update is the whole install. `upgrade` names a command each later update runs. A
  command that fails leaves the files in place, and the next update runs it again. See
  [installing with the update manager](https://github.com/ressu/orrery/blob/main/docs/configuration/update-manager.md#installing-with-the-update-manager).
- `preflight` can be a shell line, such as `"tool --check"`, which runs the new version's
  programs.
- An `executable` or `archive` entry's release file defaults to `<entry>-{arch}`, with the
  archive's `format` after it, and `asset` names another one.

### Fixed

- The `orrery` update entry offers the newest release when `~/orrery` has no
  `release_info.json`, instead of marking the install invalid.
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
