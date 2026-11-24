# Archive entries

**Decision (2026-11-23):** a new update manager entry type, `archive`, for programs released as
an archive of files, such as aldis. The files don't have to be programs. Each option says what
to expect, so the updater doesn't guess from file names (user, 2026-11-23: explicit is better
than implicit). The entry is flat: it describes the job the way the user would, and the docs
and the code group its options, not nested tables in the config (user, 2026-11-23). Orrery's
service list also gains every service that an update manager entry names in
`managed_services`, so the entry alone is enough to restart its service.

**Status (2026-11-23):** built, in `crates/orrery-updater/src/archive.rs`, with the service list
in `crates/orrery/src/services.rs` (`update_manager_units`). The differences from Moonraker are
in `update-agent` in docs/regressions.md.

## Why

aldis updates itself with `aldis self-update`, because Moonraker can't update it. Its releases
don't fit Moonraker's `executable` type:

- **Each release has one archive per CPU,** such as `aldis-aarch64-linux.tar.xz`. Inside is a
  folder, `aldis-aarch64-linux/`, with `aldis`, `LICENSE`, and `README.md`.
- **No `release_info.json`.** aldis's install unpacks the archive into `~/aldis`. An
  `executable` entry reads the installed version from that file, so it can't check aldis.
- **No checksum file.**
- **It runs as a user unit,** `~/.config/systemd/user/aldis.service`. Orrery's service list
  shows user units only for configured agents. aldis connects as an ad hoc agent, over TCP,
  without a key, so it has no `[agent.aldis]` table.

An `executable` entry with `programs` handles an archive today. But `programs` is a fixed list
of files, so a release that adds a file never installs it, and one that drops a file is
refused. That's the problem in docs/design/2026-11-22-self-update.md, "Backlog".

## The entry

The entry says what the user would say: "aldis comes from mjonuschat/aldis on GitHub, as
`aldis-<cpu>-linux.tar.xz`. It's a tar.xz, with the files inside one wrapper folder. Put them in
`~/aldis`, check that `aldis --version` runs, and restart aldis."

```toml
[update_manager.aldis]
type = "archive"
github = "mjonuschat/aldis"
asset = "aldis-{arch}-linux.tar.xz"
format = "tar.xz"
wrapper_folders = 1
path = "~/aldis"
preflight = ["aldis", "--version"]
managed_services = ["user/aldis"]
```

`{arch}` in `asset` is the printer's CPU, as `uname -m` prints it, such as `aarch64`.

The entry goes in `orrery.toml`. `moonraker.conf` holds what Moonraker accepts, and Moonraker
has no `archive` type, so an `archive` entry there is left out, with a warning, as Moonraker
would leave it out (user, 2026-11-23). The type isn't refused there; Moonraker's rules for
that file are what leave it out.

### Options

The config is flat. The docs list the options under these headings, and the code has a part
for each:

**Where the release comes from.** The option that names the source also picks it: `github`
now, and later others, such as `url`. An entry has exactly one.

| Option | Default | What it does |
|---|---|---|
| `github` | required | The GitHub repository with the releases, as `owner/name` |
| `asset` | required | The release file to download |
| `channel` | `stable` | `stable` follows the latest release; `beta` includes pre-releases |
| `checksums` | none | The release's file of SHA-256 sums, such as `SHA256SUMS`. The download must match its line |

**What's in the download.**

| Option | Default | What it does |
|---|---|---|
| `format` | required | `tar.gz` or `tar.xz`. `zip` can come later |
| `wrapper_folders` | see below | How many folders the archive wraps its files in, one inside the other |

`wrapper_folders` describes the archive, as tar's `--strip-components` counts levels, so it
doesn't depend on folder names, which often hold a version or the CPU. (User, 2026-11-23: the
name stands until a better one comes up.)

- **Unset:** an archive that holds exactly one folder at its top, and nothing beside it, has
  one wrapper folder. Any other archive has none. That's the one guess the entry makes (user,
  2026-11-23), since it fits most releases, aldis's among them.
- **Set,** to any number including `0`, it's strict: each level must be exactly one folder,
  with every file inside it. Anything else stops the update, and the error says what the
  archive holds instead, such as "The archive has its files at the top, not inside 1 wrapper
  folder." So it can't merge several folders, or flatten every folder. `0` keeps a lone
  folder as it is.

**Where it goes, and what runs it.** The options every entry type has:

| Option | Default | What it does |
|---|---|---|
| `path` | required | The folder the files go into |
| `managed_services` | none | The services to restart after an update |
| `is_system_service` | `true` | Whether the program runs as a service |
| `system_dependencies` | none | A file in `path` that lists Debian packages |
| `refresh_interval` | the global one | Hours between checks |
| `info_tags` | none | Notes that clients show with the entry |
| `report_anomalies` | `true` | Whether the entry warns about problems |

**Checks.**

| Option | Default | What it does |
|---|---|---|
| `preflight` | none | A command the new version must pass before it takes effect, as for the other types. Its first word is a file name from the archive |

There's no option to read the version by running the program (user, 2026-11-23). Tools print
their version in many forms, with or without their name, so reading it is guesswork, and a
program with a slow start makes each check slow.

## What's installed

Orrery keeps a record of the install in the entry's folder, `.release-info.json`: the
source, the release's tag, the asset, and the list of files. Its format is Moonraker's
`release_info.json` (`project_owner`, `project_name`, `version`, `asset_name`) with `files`
added. A file in the folder, not the
database, so `.previous/` can hold the record too, and a rollback brings it back with the
files. It has its own name, so it can't collide with a file in the archive, such as Orrery's
own `release_info.json`.

The version the update page shows is the tag in the record. Without a record, such as before
Orrery's first update of a program installed by hand, the version is unknown, and the entry
offers the newest release. Moonraker shows an unknown version as `?`, but clients compare
versions as semver and see no update from `?`. So the status shows `v0.0.0-missing` when the
folder isn't there, and `v0.0.0-invalid` when it is but has no record (user, 2026-11-24).
Every release is newer than both, so clients offer it, and the update manager can install a
program from nothing. Executable entries show the same, `v0.0.0-invalid` meaning no
`release_info.json`. Git entries keep `?`: they need a repository, and often a virtualenv,
set up first. A program that updates itself leaves the
record behind, so the page shows the older version until Orrery installs the next release.

Orrery's own entry is an archive entry too (since 2026-11-24): without a record in `~/orrery`,
its version is unknown in the same way.

## An update

1. **It's refused while a print runs,** as every update is.
2. **Download and check.** The asset goes to a hidden folder beside the files,
   `.<entry>.update`. With `checksums`, the download must match its line, before anything
   changes.
3. **Unpack** with the system's `tar`, as `format` says, into the hidden folder. Orrery then
   checks the wrapper folders and takes the files inside them as the new set. A link, or a
   path that leaves the folder, such as `../x`, stops the update.
4. **Preflight,** from the new set's folder.
5. **Keep the old set:** the files in the record, and the record, are hard-linked into
   `.previous/`.
6. **Put the new set in place:** each file is renamed over the old one. Files that the old set
   had and the new one doesn't are removed. Then the new record is written.
7. **Restart** the services in `managed_services`, once, after the print ends.

Before step 6, a failure leaves every file as it was. Files that Orrery never installed, such
as a config file the program writes beside itself, are never touched.

A **rollback** puts the set in `.previous/` back, and removes the files that only the newer
set had.

## The service list

Orrery's service list, which Fluidd shows and `machine.services.*` acts on, also gets each
update manager entry's own service. As in Moonraker, that's the entry's name in
`managed_services`, or, without the option, the entry's name unless `is_system_service` is
false. Another unit's name in `managed_services` stays a warning, so an entry can't make
Orrery list any unit it likes:

- **The entry's name** is a system unit, such as `crowsnest` for `crowsnest.service`. It's
  listed when systemd has it loaded, as the other system units are.
- **`user/<name>`** is a unit of Orrery's user, such as `user/aldis` for `aldis.service` in
  the `aldis` entry, as an agent's user unit is listed today. It's listed only when systemd
  has a unit file for it.
- **Orrery reads them from the config,** from both files, as it reads `[update_manager]` to
  start the updater. The entries are the admin's config, so naming a service there is
  permission enough. No `moonraker.asvc` edit is needed.
- **`setup --strict`** adds them to the polkit rule's units, as it adds `moonraker.asvc`'s.

This applies to every entry type, so a `git_repo` entry such as crowsnest no longer needs its
line in `moonraker.asvc` either. It's a difference from Moonraker, and gets a line in
`update-agent` in docs/regressions.md.

## Not in this change

- **The conversion of `moonraker.conf`.** Until orrery reads `moonraker.conf` only through the
  conversion (docs/design/2026-11-22-config-upgrade.md), the update manager checks that `type`
  came from `orrery.toml` itself. The conversion replaces that check.
- **Other sources,** such as a plain URL or another forge.
- **`zip`.**
- **Orrery's own entry** keeps its `executable` entry with the compiled-in programs. Moving it to
  `archive` answers the backlog's "A new program never arrives", but older updaters need a plan
  first.
- **The unreleased work on `executable`** from 2026-11-23 (an `asset` option, reading xz, and
  finding a wrapper folder) is replaced by this type. `executable` stays as Moonraker has it,
  plus `programs` for Orrery's own entry.

## Files in subfolders

Files in subfolders are installed too, so an archive with `lib/` works. Removing a file can
leave an empty folder, which Orrery then removes.
