# orrery's self-update

**Proposal (2026-11-22):** orrery is released on GitHub, built by CI from a tag. The update
agent gets an `orrery` entry that installs a release over the running binaries, after checking
the download and running the new version's preflight. A command-line `self-update` and
`rollback` work when orrery itself doesn't.

**Status (2026-11-22):** approved. Steps 2 to 6 are built:
- the `LICENSE` file, the build's version and release flag, and `just dist`;
- the CI workflows and the release notes in `CHANGELOG.md`, which haven't run yet, since the
  repository isn't on GitHub;
- `preflight` and `.previous/` for the existing update entries;
- archives, checksums, and a `repo` that wins, for every executable entry, and the `orrery`
  entry as an executable entry that uses them; `orrery-updater self-update` and `rollback`;
- `orrery preflight`, `orrery self-update` and `rollback`, and the warning for an agent of
  another version.

An update and a rollback ran end to end on a PC, from a shell and through a running orrery,
with a release archive and a local stand-in for GitHub. No printer has run them yet.

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
- **Versions are Cargo's three parts,** `vX.Y.Z`. Two parts, as Moonraker's, were the first
  choice, but Cargo refuses a version without its third part.
- **Preflight isn't special to orrery.** Any update entry can declare one.
- **Executable entries keep the old program** in `.previous/`, as orrery's entry does.

## Releases

- **Versions:** tags are `v0.1.0`, `v0.2.0`, and so on: the workspace version in
  `Cargo.toml`, with a `v`. CI refuses a tag that doesn't match `Cargo.toml`, and a version
  that isn't three numbers.
  - Fluidd and Mainsail offer an update only when both versions are valid semantic versions,
    which need three numbers. printerkeep learned this with its `v0.1`, which Fluidd never
    offered as an update.
  - **No prereleases,** as in printerkeep (user, 2026-11-22). A `beta` channel can come later
    if testers need one.
- **Archives:** each release has one archive per target, and one `SHA256SUMS` file for all of
  them. `orrery-aarch64.tar.gz` holds `orrery`, `orrery-import`, `orrery-spoolman`,
  `orrery-updater`, `LICENSE`, `README.md`, and a `release_info.json`. The name has no
  version, since an executable entry's `asset_name` is fixed, as in Moonraker.
  `release_info.json` names the repository, the version, and the archive, so a printer that
  unpacks the archive has an entry that can update.
- **Targets:** three, all static musl builds with rust-lld and no C toolchain. The flake's
  `armv6` target, for the Pi Zero and Pi 1, is left out: those boards are too slow for a
  modern printer (user, 2026-11-22).

  | Archive name | Rust target | For |
  |---|---|---|
  | `aarch64` | `aarch64-unknown-linux-musl` | 64-bit Raspberry Pi OS, most current boards |
  | `armv7` | `armv7-unknown-linux-musleabihf` | 32-bit Raspberry Pi OS on a Pi 2 or newer |
  | `x86_64` | `x86_64-unknown-linux-musl` | PCs |

- **Release notes,** as printerkeep writes them (user, 2026-11-22):
  - **Commit subjects are the notes.** A change users notice is a `feat:` or `fix:` commit,
    and its subject says what users get. A scope, such as `feat(updater):`, is optional and
    stays out of the notes. Other types (`docs:`, `test:`, `refactor:`, `chore:`, `ci:`,
    `release:`) stay out of the notes, however big the change.
  - **Notes are written as you go.** Each `feat:` or `fix:` commit adds its note under
    `## Unreleased` in `CHANGELOG.md`, in the same commit.
    The first release, `v0.1.0`, has no list: its notes say only "First release." (user,
    2026-11-22).
  - **A release** is one commit, `release: <version>`. It sets the version in `Cargo.toml` and
    renames `## Unreleased` to `## <version> (<date>)`. `git cliff --unreleased`, set up in
    `cliff.toml`, checks that every `feat:` and `fix:` commit has a note.
  - **The release's text** is that section, from `scripts/release-notes.sh`, with a link to
    the whole changelog for users who skip releases. The workflow checks it before the builds,
    so a release without notes fails early. Fluidd and Mainsail link to the release's page.
- **Release builds and development builds:** CI builds with `ORRERY_RELEASE=1`. A `build.rs`
  bakes that flag, the target's archive name, and the repository into each binary. The
  repository is the one CI runs in, from GitHub's `GITHUB_REPOSITORY`, so a fork's releases
  update from the fork without any config. A build without it, such
  as one from `just copy-to-printer`, is a development build:
  - `orrery --version` says "(development build)".
  - Its `orrery` entry is invalid, with the warning "This is a development build, so orrery
    doesn't update itself." A release would replace test binaries without anyone meaning it to.

## The `orrery` entry

- **It's an executable entry** for orrery's folder, with built-in settings: the archive of
  orrery's programs, `SHA256SUMS`, and a preflight of `orrery <options> preflight`. It uses
  the building blocks any executable entry has (see "Archives and checksums for every
  executable entry"); nothing in the update itself is special to orrery (user, 2026-11-22).
  What is: a development build doesn't update, the check after the restart, and orrery's
  restart coming last.
- **It's built in,** like `system`, and listed after it in `machine.update.status`.
- **One channel:** the entry follows GitHub's latest release. With no prereleases, there's
  nothing for `beta` to follow, and `dev` would need a build for each commit. A `channel`
  option for this entry is ignored, with a warning.
- **Options:** `[update_manager.orrery]` in `orrery.toml` can set `enabled` and `repo`.
  `repo` overrides the repository `release_info.json` names, for anyone who wants to follow
  their own fork (user, 2026-11-22). It's read only from `orrery.toml`, as Klipper's `origin`
  is. A fork's own releases name the fork in their `release_info.json`, so installing one with
  `self-update --repo` switches to it, with no option.
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
4. **Preflight.** The agent runs `orrery <options> preflight` from the new, still hidden
   binary, with the options orrery runs with. orrery is the agent's parent process, so the
   agent reads them from `/proc/<parent>/cmdline`. They go before `preflight`, since most of
   orrery's options aren't global. Preflight starts nothing, opens no port, and checks the
   minimum a new orrery needs to start:
   - the binary runs on this CPU;
   - `orrery.toml` and `moonraker.conf` parse;
   - the database opens, read-only, in a layout this version can read. During an update the
     running orrery holds it open, and redb lets no second process in, so the check passes
     with a note; the new orrery reads it when it starts. The check matters for a rollback
     or a self-update while orrery is stopped. A missing database passes too.

   It doesn't run the new agents: they come from the same archive, checked against its
   checksum, and CI's smoke test runs each program of a release (user, 2026-11-22: running
   every binary's `--version` was overkill). It writes nothing, and runs in orrery's folder;
   orrery's path options are made absolute against orrery's own working folder first.

   A failed preflight deletes the hidden files and stops the update, with preflight's
   message. Nothing has changed then, not even `.previous/`.
5. **Keep the old set.** The current binaries and `release_info.json` are hard-linked into
   `~/orrery/.previous/`, replacing any older copy there. The entry's `rollback_version`
   comes from that `release_info.json`, so it's right after a shell command too.
6. **Rename.** Each new binary is renamed over the old one: the agents first, `orrery` last.
   Running processes keep their old files, so nothing changes until the restart.
7. **Restart.** The agent stores the version it expects under `update_manager.orrery` in the
   database, and sends its last progress message. Then it calls `server.restart`, after every
   other restart, since that restart stops the agent. orrery starts its program again by name,
   so the new binary runs, and it starts the new agents.
8. **Check.** The new agent compares the stored version with its own. If they match, it logs
   that, and clears the record. If they don't, the entry gets a warning that says orrery runs
   another version, and how to restart it.

A full update (`machine.update.full`) skips the entry when it can't update: a development
build, or no repository. Updating it by name says why. As for executable entries, any release
other than the running one is an update, so a newer version installed with `--version` goes
back to the latest release on the next update.

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

- **If orrery answers** on its unix socket, the command asks orrery for the update, and
  prints the update dialog's messages as they come. The print check and the restart then
  work as from Fluidd. orrery's restart may close the connection before the answer; after
  the update's last message, that's success. When orrery has no update manager, the command
  goes on as if orrery didn't answer. `--version` and `--repo` always go to the updater, since
  orrery's update method takes neither.
- **If it doesn't,** the updater runs the same steps without orrery: download, check, unpack,
  preflight, and rename. It doesn't write the database, which is orrery's. It can't see
  whether a print runs, and doesn't need to: the renames change nothing for running
  processes. It prints the command that restarts orrery, and says to wait for a print to end
  first:

  ```
  sudo systemctl restart orrery
  ```

  The design first had it ask before it starts (2026-11-22); the restart is the step that
  needs the print check, and the user runs it.

- **`--version v0.2.0`** installs that release instead of the newest one. That's also a way
  back when `.previous/` is missing.

The code lives in `orrery-updater`, which gets the subcommands `self-update` and `rollback`.
`orrery self-update` and `orrery rollback` start it from orrery's folder. If the new `orrery`
binary can't run at all, the updater can be run directly, and the README says so:

```
~/orrery/orrery-updater self-update
~/orrery/.previous/orrery-updater rollback
```

## Mixed versions

Agents send their version when they connect, in `server.connection.identify`. When one of
orrery's own agents, one it starts from its own folder, has another version than orrery's,
orrery adds a warning to `server.info` until it disconnects. That catches a set that was only
partly replaced, as after a crash during step 6 or a program copied by hand. An agent from
elsewhere has its own version, and gets no warning.

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

## Archives and checksums for every executable entry

orrery's entry needs three things Moonraker's executable entries don't have. They're options
of every executable entry, read only from `orrery.toml`, since they change what runs or where
it comes from (user, 2026-11-22):

- **`checksums`** names the release's file of SHA-256 sums, such as `SHA256SUMS`. The download
  must match its line there, before anything changes. A release without the file, or without
  a line for the asset, isn't installed.
- **`programs`** makes the asset a `.tar.gz` of these programs, replaced in this order. The
  entry checks that each one is installed, instead of the asset's name. The archive's own
  `release_info.json`, when it has one, replaces the installed one, so a fork's archive brings
  the fork's repository. The preflight's first word names the program to run, from the new
  set's folder, as it's a file in the repo for a `git_repo` entry.
- **`repo`** from `orrery.toml` wins over the repository `release_info.json` names, with no
  anomaly. From `moonraker.conf`, it's Moonraker's option: `release_info.json` wins, and a
  mismatch is an anomaly.

```toml
[update_manager.tool]
type = "executable"
path = "~/tool"
repo = "example/tool"
checksums = "SHA256SUMS"
programs = ["tool-helper", "tool"]
preflight = ["tool", "--check"]
```

Every update, with or without these options, downloads into a hidden folder beside the
programs, `.<entry>.update`, checks and unpacks there, runs the preflight there, and then
renames. A failure before the renames removes the folder, and nothing else changes.

## `.previous/` for executable entries

Today an executable entry keeps no copy of the program it replaced: a rollback downloads that
release again by its tag.

- **An update** hard-links the programs and their `release_info.json` into
  `<path>/.previous/` before the renames.
- **A rollback** renames them back, with no network access, whenever `.previous/` holds them.
  Its version wins over the database's rollback version, since a shell command changes it
  without the database. Without `.previous/`, it downloads by tag, as now.
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
2. A release's archive is checked against its SHA-256 sum before anything changes. Any
   executable entry can ask for that, and for an archive of programs, in `orrery.toml`.
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
7. Docs, then the first release, `v0.1.0`, installed on bluev2 with `orrery setup` run again.
   Then `v0.2.0`, for a real self-update and a rollback.
