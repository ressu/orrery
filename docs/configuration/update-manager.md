# The update manager

The update manager updates Klipper, Orrery, and other software on the printer, from Fluidd's
and Mainsail's update pages. It also updates the system's Debian packages.

It checks for updates once a week, and never installs one by itself. It refuses to update while
a print runs.

## Turning it on

Add `[update_manager]` to `orrery.toml`:

```toml
[update_manager]
```

A printer that came from Moonraker can keep its `[update_manager]` sections in
`moonraker.conf`, and Orrery reads them there. When both files have an option, `orrery.toml`
wins.

The update manager is a separate program, `orrery-updater`, in the same folder as Orrery.
Orrery starts it when either file has `[update_manager]` or an `[update_manager <name>]` entry.

To install Debian packages, Orrery needs a permission that setup gives it. If you set up Orrery
before it had the update manager, run setup again:

```
sudo ~/orrery/orrery setup
```

## Options

These go in `[update_manager]`:

| Option | Default | What it does |
|---|---|---|
| `refresh_interval` | `168` | Hours between checks for updates. 168 is a week |
| `enable_auto_refresh` | `true` | With `false`, Orrery checks only when it starts, or when you press refresh |
| `refresh_window` | none | Hours of the day when checks may run, such as `0-5` for midnight to 5 am |
| `channel` | none | The default channel for every entry: `stable`, `beta`, or `dev` |
| `enable_system_updates` | `true` | Whether the Debian packages are listed |

## Entries

The update manager lists these entries by itself:

- **Klipper or Kalico,** named after the one that runs.
- **orrery.**
- **The system's Debian packages,** named `system`.

It has no `moonraker` entry. Each other program gets an `[update_manager <name>]` section, or
`[update_manager.<name>]` in `orrery.toml`. Plugin installers, such as crowsnest's, add theirs
to `moonraker.conf`, and Orrery reads them there.

```toml
[update_manager.crowsnest]
type = "git_repo"
path = "~/crowsnest"
origin = "https://github.com/mainsail-crew/crowsnest.git"
managed_services = ["crowsnest"]

[update_manager.printerkeep]
type = "executable"
channel = "stable"
repo = "ressu/printerkeep"
path = "~/printerkeep"
is_system_service = false
```

Orrery updates `git_repo` and `executable` entries, and `archive` entries, a type of its own.
It doesn't update `web` and `zip` entries yet, and lists a warning for each one it leaves out.

### Options for every entry

| Option | Default | What it does |
|---|---|---|
| `type` | required | `git_repo`, `executable`, or `archive` |
| `path` | required | Where the program is |
| `channel` | `dev` for `git_repo`, `stable` otherwise | `stable` and `beta` follow releases; `dev` follows the branch |
| `refresh_interval` | the global one | Hours between checks for this entry |
| `managed_services` | none | The services to restart after an update; see [Restarts](#restarts) |
| `is_system_service` | `true` | Whether the program runs as a service |
| `info_tags` | none | Notes that clients show with the entry |
| `report_anomalies` | `true` | Whether the entry warns about problems, such as an unexpected remote |
| `preflight` | none | A check the new version must pass first; see [Checks before an update](#checks-before-an-update) |

### `git_repo` entries

| Option | Default | What it does |
|---|---|---|
| `origin` | required | The repository's address |
| `primary_branch` | `master` | The branch to follow |
| `moved_origin` | none | An old address of the repository, which Orrery replaces with `origin` |
| `pinned_commit` | none | A commit to stay on |
| `virtualenv` | none | The Python environment to install `requirements` into |
| `requirements` | none | A Python requirements file in the repository |
| `system_dependencies` | none | A file in the repository that lists Debian packages |
| `install_script` | none | A script in the repository that installs what the program needs |

A remote that starts with `http://` or `git://` is reported as a problem, since the connection
isn't encrypted.

### `executable` entries

| Option | Default | What it does |
|---|---|---|
| `repo` | required | The GitHub repository that has the releases, as `owner/name` |
| `system_dependencies` | none | A file that lists Debian packages |
| `checksums` | none | A file in the release with SHA-256 sums, such as `SHA256SUMS`, that the download must match |
| `programs` | none | The programs in the release's `.tar.gz`. Without it, the release has one program |

`checksums` and `programs` work only in `orrery.toml`. `repo` in `orrery.toml` wins over the
one the program's `release_info.json` names. An update keeps the program it replaced in
`.previous/` beside it, so a rollback needs no download.

```toml
[update_manager.tool]
checksums = "SHA256SUMS"
programs = ["tool-helper", "tool"]
preflight = ["tool", "--check"]
```

### `archive` entries

An `archive` entry installs every file of a release's archive into `path`. It suits programs
that ship as an archive without a `release_info.json`, such as aldis. Write it in
`orrery.toml`. Moonraker has no `archive` type, and Orrery reads `moonraker.conf` as Moonraker
does, so it leaves an `archive` entry there out, with a warning.

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

Where the release comes from:

| Option | Default | What it does |
|---|---|---|
| `github` | required | The GitHub repository with the releases, as `owner/name` |
| `asset` | required | The release's file to download. `{arch}` is the printer's CPU, as `uname -m` prints it, such as `aarch64` |
| `checksums` | none | A file in the release with SHA-256 sums, such as `SHA256SUMS`, that the download must match |

What's in the download:

| Option | Default | What it does |
|---|---|---|
| `format` | required | `tar.gz` or `tar.xz` |
| `wrapper_folders` | see below | How many folders the archive wraps its files in, one inside the other |

When `wrapper_folders` isn't set, an archive that holds only one folder has one wrapper
folder, and any other archive has none. When it's set, including to `0`, the archive must
match it, or the update stops and says what the archive holds.

Orrery keeps a list of the files it installed, in `.orrery-install.json` in `path`. An update
removes the files that the new release no longer has, and leaves alone any file Orrery didn't
install, such as a config file. The old files stay in `.previous/`, so a rollback needs no
download.

The version on the update page is the release Orrery installed. Before Orrery's first update,
such as for a program you installed by hand, the version is `?`, and the entry offers the
newest release. If the program updates itself, the page shows the older version until Orrery
installs the next release.

## Klipper's entry

Klipper's entry expects the project's own repository and branch. Other ones are reported as a
problem. To run a fork on purpose, name it in `orrery.toml`, and the entry reports no problem:

```toml
[update_manager.klipper]
origin = "https://github.com/someone/klipper"
primary_branch = "main"
```

The section is `[update_manager.kalico]` for Kalico. It also takes `channel`, `pinned_commit`,
`refresh_interval`, `report_anomalies`, `preflight`, and `enabled = false`, which hides the
entry. `origin`, `primary_branch`, and `moved_origin` work only in `orrery.toml`.

Plugins linked into Klipper's folder belong to their own entries, and updating one restarts
Klipper. Klipper's update stops before it overwrites a plugin's file, and says which.

## Orrery's own entry

The `orrery` entry installs Orrery's releases from GitHub, then restarts Orrery. The old
programs stay in `~/orrery/.previous/`, and a rollback puts them back. A build of your own
doesn't update itself. If `~/orrery` has no `release_info.json`, the version is `?`, and the
entry offers the newest release.

To take releases from a fork, name it in `orrery.toml`:

```toml
[update_manager.orrery]
repo = "someone/orrery"
```

`enabled = false` hides the entry.

## Checks before an update

An entry in `orrery.toml` can name a check that the new version must pass before the update
takes effect:

- For an `executable`, `preflight` is the arguments to the new program, such as
  `["--version"]`. With `programs`, the first word names the program to run.
- For a `git_repo`, it's a file in the repository and its arguments.
- For an `archive`, it's a file of the archive and its arguments.

```toml
[update_manager.printerkeep]
preflight = ["--version"]
```

If the check fails, the old version stays, and the update dialog shows the check's output.
Orrery's own entry always runs `orrery preflight`.

## Restarts

After an update, Orrery restarts each service in `managed_services` once. If a print is
running, the restarts wait until it ends.

As in Moonraker, `managed_services` can name the entry's own service, `klipper`, and
`moonraker`. Without the option, an entry restarts its own service, unless
`is_system_service = false`. Orrery adds `user/<name>`, for a service of Orrery's user that
runs with `systemctl --user`, such as `user/aldis` for the `aldis` entry.

Orrery lists each entry's own service on the Services page, so you can restart it from there
too. You don't need to add it to `moonraker.asvc`. If you set up Orrery with `--strict`, run
setup again after you add an entry, so polkit allows its service.

`moonraker` in `managed_services` restarts nothing, since Orrery isn't Moonraker.

For how the update manager differs from Moonraker's, see `update-agent` and `update-safety` in
[Known differences from Moonraker](../regressions.md).
