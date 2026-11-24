# Update entries that install themselves

**Decision (2026-11-25):** An update manager entry can say that the updater
may install what it describes, from nothing. It can also name a command to run after that
first install, and another to run after each upgrade. A user then adds the entry and presses
update, with no separate installer and no `curl | sudo sh` (user, 2026-11-24). Nothing here is
built yet.

## Why

An entry today only updates what something else installed:

- **`executable`:** Moonraker's docs say the program's own installer "MUST create
  `release_info.json`", and Moonraker checks it on each refresh. Without it, the entry is
  invalid and refuses to update. Only Moonraker's `[analysis]` section writes one itself, for
  Klipper Estimator.
- **`git_repo`:** without a repository the entry is invalid. Moonraker's hard recover refuses
  with "Recovery url has not been detected". Orrery's clones the configured `origin`
  (`git.rs`, `hard_recover`), a difference from Moonraker that stays (see "Recover").
- **`archive`:** installs into a missing folder already, without being asked.

Not every program works as placed. Some need a setup step, so a bare install can't be the only
form. Moonraker's `install_script` sounds like that step, but neither Moonraker nor Orrery runs
it; both only read its `PKGLIST="…"` lines for system packages. The only command Orrery runs for
an entry today is `preflight`.

The person who writes the entry, usually the program's author, knows whether the updater can
install it. So the entry says so; the updater never guesses.

## Options

Both are `orrery.toml` options. `moonraker.conf` has no such options, so there they're ignored
with a warning, as `preflight` is.

| Option | Value | Meaning |
|---|---|---|
| `install` | absent or `false` | The updater doesn't install it. |
| | `true` | It installs the release as is: the files are all the program needs. |
| | a command | It installs the release, then runs the command. |
| `upgrade` | absent | An upgrade needs no step. |
| | a command | Each upgrade runs it. |

A command is written the same way for `install`, `upgrade` and `preflight`:

- **A shell line:** `"./install.sh --unattended"`, run with `sh -c`.
- **A list of words:** `["./install.sh", "--unattended"]`, run without a shell.

`preflight` gains the shell line; its list stays as it is.

```toml
[update_manager.tool]
type = "git_repo"
path = "~/tool"
origin = "https://github.com/example/tool.git"
install = "./install.sh --unattended"
upgrade = ["./scripts/migrate.sh"]

[update_manager.monitor]
type = "archive"
github = "example/monitor"
path = "~/monitor"
format = "tar.gz"
install = true
```

Each command runs only on its own event: a fresh install runs `install`, an upgrade runs
`upgrade`, and a rollback runs neither. One doesn't stand in for the other; a program that
needs the same step both times names it twice.

## What counts as a fresh install

| Type | Fresh install | Otherwise |
|---|---|---|
| `archive` | No folder, or a folder without the updater's record, `.release-info.json` | An upgrade |
| `executable` | No folder, or a folder without `release_info.json` | An upgrade |
| `git_repo` | No folder | A folder without a repository, empty or not, is an error the updater can't fix |

## The update page

| State | With `install` | Without |
|---|---|---|
| No folder | `v0.0.0-missing` | `?` |
| A folder without a record | `v0.0.0-invalid` | `?` |
| The last command failed | `v0.0.0-invalid`, and a warning | — |

Clients compare versions as semver. Any release is newer than `v0.0.0-missing` and
`v0.0.0-invalid`, so Fluidd and Mainsail offer the update, and the update installs. `?` offers
nothing: the updater can't fix that entry. This replaces today's rule, where every `archive`
and `executable` entry shows `v0.0.0-missing` or `v0.0.0-invalid`.

A git entry without a repository has no current commit. Its refresh reads the remote branch's
commit with `git ls-remote`, so the page has an update to offer.

## The steps

Preflight keeps its rule: it checks the new version before it takes effect, and a failure puts
the old one back. A release can be checked before its files take their place. A git pull can
only be checked after it, before the restart. `upgrade` runs after preflight passes, so a failed
check never leaves a half-migrated install.

| Step | Release upgrade | Git upgrade | Fresh install |
|---|---|---|---|
| 1 | Download | Pull | Download, or clone |
| 2 | Preflight on the downloaded files | System packages | Place the files |
| 3 | Place the files, the old ones to `.previous/` | Python packages | System packages |
| 4 | System packages | Preflight; a failure resets to the old commit | `install` |
| 5 | `upgrade` | `upgrade` | Python packages |
| 6 | Restart | Restart | Restart |

A fresh install has no preflight: nothing runs yet that needs protecting, and `install` is what
makes the program work. `install` runs before the Python packages, since it may make the
virtualenv.

## Running a command

- In the entry's folder, as Orrery's user, with stdin closed.
- Its output goes to the update dialog line by line.
- The environment has `ORRERY_ENTRY`, `ORRERY_PATH`, `ORRERY_VERSION`, and
  `ORRERY_PREVIOUS_VERSION`, which is empty for an install.
- As Orrery's user, never as root (user, 2026-11-25). Orrery doesn't support `sudo` in a
  command, but doesn't try to block it either: that would never end. A step that needs root is
  done by hand, outside the updater. System packages come from the entry's
  `system_dependencies`, which the updater installs through PackageKit first.
- A time limit of 30 minutes (user, 2026-11-25). No command should come close, but an explicit
  limit means a hung one ends. Preflight's 60 seconds is too short for a build or pip.

## When a command fails

The files stay in place. Undoing a fresh install leaves nothing to look at, and the next update
runs the command again:

- **During the update:** the dialog shows the command's output, and the update ends with an
  error that names the command, its exit status, and its last lines, as a failed preflight's
  does.
- **Afterwards:** the entry stays valid, so Fluidd keeps its update button; an invalid entry
  shows only "Invalid". Its version shows `v0.0.0-invalid`, and it has a warning, which Fluidd
  shows under the entry: "The install command `./install.sh` failed with exit status 1. Update
  again to run it again."
- **The next update** runs only the failed command when the version hasn't changed, without
  downloading again. An install that failed stays an install, so it runs `install`, not
  `upgrade`.
- The failed command is kept in the entry's state in the database, the same for every type,
  so it survives a restart.

## Executables

The updater writes `release_info.json` after a fresh install, from what it installed: the
owner, project and tag of the GitHub release, and the asset's name. After that, the folder is an
ordinary Moonraker install, and Moonraker could take it over.

Which file of the release to download, first match wins (user, 2026-11-25):

1. **`asset` in the entry,** with `{arch}` for the CPU as `uname -m` names it. It's for a
   release whose files are named differently, or a project that renames them: it wins over
   `release_info.json`, and the update writes the new name there.
2. **`asset_name` in `release_info.json`,** as in Moonraker.
3. **`<entry>-{arch}`,** where `<entry>` is the entry's name. For `[update_manager.tool]` on a
   64-bit ARM computer, that's `tool-aarch64`.

The default costs nothing and changes nothing for a Moonraker entry: one with
`release_info.json` still uses its `asset_name`, and one without it is still invalid, since
without `install` the updater doesn't install it. The one difference is a `release_info.json`
without `asset_name`: Moonraker calls the entry invalid, and Orrery uses the default.

## Assets of archive entries

The same order, with the updater's record in place of `release_info.json`, and the default
ending in the entry's `format`: `<entry>-{arch}.tar.gz` or `<entry>-{arch}.tar.xz`. `asset`
stops being required.

## Orrery's own entry

It has `install = true` built in, and no `upgrade`. An install from v0.1.2 or older has no
record, so the first update installs every file of the release, as it does now.

## Not covered

- **Commands as root,** on purpose, for safety. A program whose install needs root can't be
  installed to the end by the updater: aldis's entry can use `install = true` for its files,
  but `aldis setup`, which needs root, is still run by hand.
- **`install_script`** keeps Moonraker's meaning. The docs say it isn't run, and point to
  `install`.

## Recover

A hard recover repairs a git entry by cloning its repository again, with or without `install`,
as in Moonraker (user, 2026-11-25). An install script often changes things outside the
repository, such as a service file or a virtualenv, so a new clone of the repository is safe to
make without it. Orrery also clones when the repository is missing, from the entry's `origin`,
where Moonraker refuses because it has no remote to read the URL from.

With `install`, a hard recover runs `install` after the clone, since a new clone is a fresh
install of the repository. Its system and Python packages follow, as in the table above. A soft
recover only resets the repository, so it runs neither command.
