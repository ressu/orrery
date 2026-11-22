# orrery's self-update

**Proposal (2026-11-22):** orrery is released on GitHub, built by CI from a tag. The update
agent gets an `orrery` entry that installs a release over the running binaries, after checking
the download and running the new version's preflight. A command-line `self-update` and
`rollback` work when orrery itself doesn't.

**Status (2026-11-22):** approved. Nothing is built yet.

## What exists today

- **No releases.** The binaries reach a printer through `just copy-to-printer`, which builds
  them locally and copies them to `~/orrery-test/bin`. bluev2 runs a copy in `~/orrery/`.
- **No `orrery` entry.** `docs/regressions.md` says orrery's own updates wait for a release
  source. `machine.update.moonraker` is refused.
- **`orrery setup`** writes `orrery.service` with `ExecStart` set to the orrery binary it was
  run from, such as `~/orrery/orrery`. orrery starts its agents from the same folder.
- **Executable entries,** such as printerkeep, already update from GitHub releases. A new
  program goes to a hidden file beside the old one and is renamed over it. A rollback
  downloads the replaced release again by its tag.
- **The repository has no `LICENSE` file,** though `Cargo.toml` says GPL-3.0-only, as
  Moonraker's `pyproject.toml` does. Klipper and Kalico are under GPLv3 too.

## Decisions (user, 2026-11-22)

- **GitHub** hosts the repository and the releases. The update agent already has a GitHub
  client, with its rate limits, for executable entries.
- **CI builds a release when a tag is pushed.**
- **SHA-256 checksums, over HTTPS,** check the download. There are no signatures for now:
  printers are commonly set up with `curl | sudo sh`, so a signature isn't the weak point.
  Stricter checks can come later.
- **The binaries are replaced in place,** in `~/orrery`. `ExecStart` and `orrery setup` don't
  change. Version folders with a `current` link were considered and dropped: they'd need
  more logic to decide which path goes into the unit, for little gain on a printer.
- **Versions have two parts,** `vX.Y`, as Moonraker's do.
- **Preflight isn't special to orrery.** Any update entry can declare one.
- **Executable entries keep the old program** in `.previous/`, as orrery's entry does.

## Releases

- **Versions:** tags are `v0.1`, `v0.2`, and so on. A tag with a suffix, such as `v0.2-rc.1`,
  is a GitHub prerelease.
  - Cargo needs three parts, so the workspace version in `Cargo.toml` is `0.2.0`, and its
    last part is always 0.
  - CI refuses a tag that doesn't match `Cargo.toml`, and a version whose last part isn't 0.
  - `orrery --version` and the update entry show `v0.2`.
- **Archives:** each release has one archive per target, and one `SHA256SUMS` file for all of
  them. `orrery-v0.2-aarch64.tar.gz` holds `orrery`, `orrery-import`, `orrery-spoolman`,
  `orrery-updater`, `LICENSE`, and `README.md`.
- **Targets:** three, all static musl builds with rust-lld and no C toolchain. The flake's
  `armv6` target, for the Pi Zero and Pi 1, is left out: those boards are too slow for a
  modern printer (user, 2026-11-22).

  | Archive name | Rust target | For |
  |---|---|---|
  | `aarch64` | `aarch64-unknown-linux-musl` | 64-bit Raspberry Pi OS, most current boards |
  | `armv7` | `armv7-unknown-linux-musleabihf` | 32-bit Raspberry Pi OS on a Pi 2 or newer |
  | `x86_64` | `x86_64-unknown-linux-musl` | PCs |

- **Release notes:** GitHub generates them from the commits. They can be edited on GitHub
  after the release.
- **Release builds and development builds:** CI builds with `ORRERY_RELEASE=1`. A `build.rs`
  bakes that flag, the target's archive name, and the repository into each binary. The
  repository is the one CI runs in, from GitHub's `GITHUB_REPOSITORY`, so a fork's releases
  update from the fork without any config. A build without it, such
  as one from `just copy-to-printer`, is a development build:
  - `orrery --version` says "(development build)".
  - Its `orrery` entry is invalid, with the warning "This is a development build, so orrery
    doesn't update itself." A release would replace test binaries without anyone meaning it to.

## The `orrery` entry

- **It's built in,** like `system`, and listed after it in `machine.update.status`.
- **Channels:** `stable` follows GitHub's latest release, and `beta` takes prereleases too.
  The default is `stable`. `dev` isn't offered: it would need a build for each commit.
- **Options:** `[update_manager.orrery]` in `orrery.toml` can set `channel`, `enabled`, and
  `repo`. `repo` overrides the repository the build names, for anyone who wants to follow
  their own fork (user, 2026-11-22). It's read only from `orrery.toml`, as Klipper's `origin`
  is.
- **Clients:** Fluidd's Update button calls `machine.update.client` for any entry other than
  `klipper` and `moonraker`, and `client` updates any entry, so Fluidd works as it is.
  `machine.update.moonraker` stays refused: orrery doesn't update Moonraker.
- **Status:** the same fields as an executable entry: `version`, `remote_version`,
  `rollback_version`, `release_notes`, and GitHub's rate limits.

## An update

1. **It's refused while a print runs,** as every update is.
2. **Download and check.** The agent downloads its target's archive and `SHA256SUMS`, and
   checks the archive's sum. A wrong or missing sum stops the update; nothing has changed.
3. **Unpack.** Each binary goes to a hidden file in `~/orrery`, on the same file system as the
   binary it replaces, with that binary's permissions.
4. **Keep the old set.** The current binaries are hard-linked into `~/orrery/.previous/`,
   replacing any older copy there.
5. **Preflight.** The agent runs `orrery preflight` from the new, still hidden binary, with
   the options orrery runs with. orrery is the agent's parent process, so the agent reads them
   from `/proc/<parent>/cmdline`. Preflight starts nothing, opens no port, and checks the
   minimum a new orrery needs to start:
   - the binary runs on this CPU;
   - each new agent binary answers `--version` with the same version;
   - `orrery.toml` and `moonraker.conf` parse;
   - the database opens, read-only, in a layout this version can read.

   A failed preflight deletes the hidden files and stops the update, with preflight's
   message. Nothing has changed then.
6. **Rename.** Each new binary is renamed over the old one: the agents first, `orrery` last.
   Running processes keep their old files, so nothing changes until the restart.
7. **Restart.** The agent stores the version it expects under `update_manager.orrery` in the
   database, sends its last progress message, and calls `server.restart`. systemd starts the
   new orrery, which starts the new agents.
8. **Check.** The new agent compares the stored version with its own. If they match, it sends
   "orrery updated to v0.2" and clears the record. If they don't, the entry gets a warning.

## Rollback

- **`machine.update.rollback`** with `name=orrery` renames the binaries in `.previous/` back,
  after the same preflight, then restarts orrery.
- **`orrery rollback`** does the same from a shell, for a version that won't start or won't
  answer. The update's progress messages name this command.
- **Without `.previous/`,** a rollback is refused. `orrery self-update --version` installs a
  given release instead.
- **The database isn't rolled back.** From now on, a release that changes the database's
  layout copies the database first, so a rollback can put it back. No release changes it yet.
- **No automatic rollback yet.** If the new orrery doesn't start, systemd keeps restarting it
  until someone runs `orrery rollback`. Rolling back by itself is later work.

## From a shell

`orrery self-update` is for a printer whose orrery won't start, or won't answer:

- **If orrery answers** on its unix socket, the command asks orrery for the update. The print
  check and the restart then work as from Fluidd.
- **If it doesn't,** the updater runs the same steps without orrery: download, check, unpack,
  preflight, and rename. It can't see whether a print runs, so it says so and asks before it
  starts. Then it prints the command that restarts orrery:

  ```
  sudo systemctl restart orrery
  ```

- **`--version v0.2`** installs that release instead of the newest one. That's also a way
  back when `.previous/` is missing.

The code lives in `orrery-updater`, which gets the subcommands `self-update` and `rollback`.
`orrery self-update` and `orrery rollback` start it from orrery's folder. If the new `orrery`
binary can't run at all, the updater can be run directly, and the README says so:

```
~/orrery/orrery-updater self-update
~/orrery/.previous/orrery-updater rollback
```

## Mixed versions

Agents send their version when they connect. orrery adds a warning to `server.info` when an
agent's version differs from its own. That catches a set that was only partly replaced, such
as after a crash during step 6.

## Preflight for every entry

Any entry can declare a preflight, and orrery's entry declares `orrery preflight` itself.

- **The option is `preflight`,** a command as a list of words. It's orrery's own option, and
  it's read only from `orrery.toml`: it runs a command, and a config written for Moonraker
  mustn't decide that.
- **For an `executable` entry,** the words are arguments to the new program, run from its
  hidden file before the rename:

  ```toml
  [update_manager.printerkeep]
  preflight = ["--check"]
  ```

  A failure deletes the hidden file, and the old program stays.
- **For a `git_repo` entry,** the first word is a file in the repo. It runs in the repo after
  the fast-forward and the Python packages, before any restart:

  ```toml
  [update_manager.my_plugin]
  preflight = ["scripts/check.sh"]
  ```

  A failure resets the repo to the commit it had and skips the restart. Python packages that
  the update installed stay installed, and the message says so.
- **Every check** runs as orrery's user, with a 60-second limit. Exit status 0 passes. A
  failure's message ends with the last lines of the check's output.

## `.previous/` for executable entries

Today an executable entry keeps no copy of the program it replaced: a rollback downloads that
release again by its tag.

- **An update** hard-links the program and its `release_info.json` into `<path>/.previous/`
  before the rename.
- **A rollback** renames them back, with no network access. Without `.previous/`, it downloads
  by tag, as now.
- **This reverses a line in the update manager's note,** which stored only the rollback
  version so an update wouldn't double the disk use. Each executable entry now keeps one old
  copy. For printerkeep that's a few MB.

## Permissions

The agent runs as orrery's user, who owns `~/orrery`. Self-update needs no new polkit actions
and no sudo. Restarting orrery from a shell, after a self-update without orrery, needs sudo or
the `orrery-admin` group, as any service restart does.

## Installing the first time

The README explains the steps:

1. Download the archive for the printer's CPU from the latest release.
2. Unpack it into `~/orrery`.
3. Run `sudo ~/orrery/orrery setup`.

An install script, for `curl | sh`, comes later.

## Differences from Moonraker

1. Moonraker updates itself as a `git_repo` entry named `moonraker`. orrery's entry is named
   `orrery`, and installs a release.
2. A release's archive is checked against its SHA-256 sum before anything changes.
3. The new version's preflight runs before it replaces the old one.
4. `orrery rollback` and `orrery self-update` work from a shell, while orrery is stopped.
5. Any entry can declare a `preflight`, in `orrery.toml`.
6. Executable entries keep the program they replaced, in `.previous/`.

## The order of work

1. This note, reviewed.
2. Release builds:
   - The `LICENSE` file: the GPL version 3 text, copied from Moonraker's.
   - A README note on where orrery comes from: a Moonraker-compatible API server, heavily
     inspired by Moonraker, which is copyright Eric Callahan (Arksine) and its contributors,
     under the same license.
   - `build.rs` with the release flag, the target, and the repository.
   - A `just dist <target>` recipe that writes the archive and its `SHA256SUMS` line.
3. CI: tests on every push, and the release workflow on a tag.
4. Preflight and `.previous/` for existing entries.
5. The `orrery` entry, with download, check, unpack, preflight, rename, and restart.
6. orrery itself: `orrery preflight`, `self-update`, and `rollback`, and the agents' versions.
7. Docs, then the first release, `v0.1`, installed on bluev2 with `orrery setup` run again.
   Then `v0.2`, for a real self-update and a rollback.
