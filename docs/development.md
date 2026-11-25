# Developing Orrery

This page is for working on Orrery itself: the code layout, building, testing on a printer, and
making a release.

## How Orrery is tested

The work starts with a conformance suite that checks Moonraker's API from the outside. Upstream
Moonraker is the reference: a case is correct when it passes against Moonraker, and Orrery
must pass the same cases. See [conformance/README.md](../conformance/README.md).

For the plan and the decisions made so far, see
[the Orrery plan](design/2026-11-01-orrery-plan.md). The design notes in `docs/design/` explain
the larger features.

## Layout

| Path | Contents |
|---|---|
| `crates/orrery` | The server, `orrery`, and the database import, `orrery-import` |
| `crates/orrery-agent` | A client for writing agents in Rust; see its `examples/hello.rs` |
| `crates/orrery-gpio` | Buttons on the printer computer's GPIO lines, as an agent |
| `crates/orrery-spoolman` | Moonraker's Spoolman integration, as an agent |
| `crates/orrery-updater` | Moonraker's update manager, as an agent |
| `crates/orrery-dbus` | A small D-Bus client, for systemd and PackageKit |
| `crates/orrery-proto` | Shared wire formats: Klippy socket framing and JSON-RPC envelopes |
| `crates/orrery-build` | What a build is: its version, whether it's a release, and its target |
| `crates/fake-klippy` | A scriptable stand-in for Klipper's API socket |
| `crates/conformance` | The conformance test runner |
| `conformance/` | Test cases, fixtures, and recorded snapshots |
| `scripts/` | The installer, release archives, and copying builds to a printer |
| `docs/comparison.md` | What you lose, gain, and should look out for, compared with Moonraker |
| `docs/regressions.md` | Each difference from Moonraker that a conformance case checks |
| `docs/oddities.md` | Moonraker behaviour that looks odd but that Orrery keeps on purpose |
| `docs/design/` | The plan, its decision log, and design notes |
| `spikes/` | Throwaway experiments, each a standalone crate outside the workspace |

## Setting up

The development tools come from a nix flake. With direnv, they load when you enter the folder.
Without direnv, run `nix develop`.

To set up the reference Moonraker and Klipper, then run everything:

```
just ref-setup
just klipper-setup
just
```

By default the recipes copy Moonraker and Klipper from `~/sources/moonraker` and
`~/sources/klipper`. To use other checkouts, set `MOONRAKER_SRC` or `KLIPPER_SRC`. The
originals are never changed.

## Building for a printer

To build release programs for ARM single-board computers, 64-bit and 32-bit, run:

```
just build-arm
```

The programs link with `rust-lld` and Rust's bundled musl, so they need no C cross toolchain.

### Copying a build to a printer

To build Orrery, `orrery-gpio`, `orrery-import`, `orrery-spoolman`, and `orrery-updater` for a
printer's CPU and copy them over SSH, run:

```
scripts/copy-to-printer.sh <ssh host>
```

`<ssh host>` is what you'd give `ssh`, such as `pi@printer-1`. The programs go to
`~/orrery-test/bin` on the printer, and each is copied only when it changed. The script doesn't
start Orrery or change anything else on the printer. To close the SSH connection it keeps open
for 10 minutes, run `scripts/copy-to-printer.sh --disconnect <ssh host>`.

A build of your own doesn't update itself.

### Running a build on a printer

To run the copied build as a temporary systemd unit, `orrery-test`, run:

```
just printer-test start <ssh host>
```

While it runs, Moonraker and an installed `orrery.service` are stopped. The unit runs as your
user with the `moonraker-admin` group, so Orrery can manage services through Moonraker's polkit
rule. No unit file is written. Options after the host go to Orrery, such as
`just printer-test start <ssh host> --verbose`.

| Command | What it does |
|---|---|
| `just printer-test log <ssh host>` | Follows the log |
| `just printer-test stop <ssh host>` | Stops the unit, and starts again what it stopped |

### Keeping one SSH connection open

To log in once and keep the connection open while you work, run:

```
just connect-printer <ssh host>
```

Later `ssh`, `scp`, and `copy-to-printer` runs share it. It closes after 12 hours unused;
`just connect-printer <ssh host> 1h` sets another time. To close it, run:

```
just disconnect-printer <ssh host>
```

## Commits and the changelog

GitHub Actions checks every push: format, lint, unit tests, the conformance cases against
Orrery, and the shell scripts.

**Commit subjects are the release notes.** A change users notice is a `feat:` or `fix:` commit,
such as `feat(updater): Kalico's update stops before it overwrites a plugin's link`. The scope
is optional. The same commit adds its note under `## Unreleased` in `CHANGELOG.md`: what users
get, and what they notice after the update.

Other changes are `docs:`, `test:`, `refactor:`, `ci:`, or `chore:`, and stay out of the notes.
When users must do something after the update, mark the commit `feat!:`, and say what in a
`BREAKING CHANGE:` footer.

## Making a release

1. Set the new version in `Cargo.toml`, under `[workspace.package]`, such as `0.2.0`. Then run
   `cargo check` to update `Cargo.lock`.
2. In `CHANGELOG.md`, rename `## Unreleased` to `## <version> (<date>)`. Check that each `feat:`
   and `fix:` commit has a note there:

   ```
   git cliff --unreleased
   ```

   To see the release's text, run `scripts/release-notes.sh <version>`.
3. Commit as `release: <version>`, and push. Then tag `v<version>`, and push the tag:

   ```
   git tag v0.2.0
   git push origin v0.2.0
   ```

The release workflow refuses a tag that doesn't match `Cargo.toml`, and a version without
notes. It runs the checks, builds an archive for `aarch64`, `armv7`, and `x86_64`, tries each
one, and publishes them with their `SHA256SUMS` and the version's notes. "Run workflow" on the
workflow's page builds and tests the archives without making a release.

To build an archive locally, run:

```
just dist <arch>
```

`<arch>` is `aarch64`, `armv7`, or `x86_64`. The archive and its checksum go to `dist/`.
