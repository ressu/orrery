# orrery

orrery is an experimental API server for Klipper that aims to work as a drop-in replacement for
[Moonraker](https://github.com/Arksine/moonraker), so that Fluidd, Mainsail, and KlipperScreen work with
it unchanged. It's written in Rust and builds as one static binary for the Raspberry Pi.

"orrery" is a placeholder name. The long-term goal is to try out ideas Moonraker doesn't have,
starting with per-user state.

## Status

The work starts with a conformance suite that checks Moonraker's API from the outside. Upstream
Moonraker serves as the reference implementation.

orrery connects to Klippy and serves the `server.*`, `printer.*`, `server.database.*`,
`server.files.*`, `server.history.*`, `server.job_queue.*`, `server.webcams.*`,
`machine.system_info`, and `machine.proc_stats` APIs, with file upload and download.
Authorization isn't written yet. **orrery has no authorization yet, so every client that can
reach it has full access.** See
[docs/regressions.md](docs/regressions.md).
For the plan and the decisions made so far, see
[docs/design/2026-11-01-orrery-plan.md](docs/design/2026-11-01-orrery-plan.md).

## Moving from Moonraker

`orrery-import` copies Moonraker's settings, webcams, and print history into orrery's store. It
only reads Moonraker's database, so Moonraker keeps working as before.

1. Stop Moonraker and orrery.
2. Copy the database, so the import never reads the file Moonraker uses:

   ```
   cp ~/printer_data/database/moonraker-sql.db ~/moonraker-sql.copy.db
   ```

3. See what the import would change:

   ```
   orrery-import --dry-run ~/moonraker-sql.copy.db
   ```

4. Import, then start orrery:

   ```
   orrery-import ~/moonraker-sql.copy.db
   ```

Running the import again changes nothing, and print jobs orrery recorded itself are kept. To
start over from the copy, add `--replace`: it first removes everything earlier imports wrote.
Users aren't imported yet. See [docs/regressions.md](docs/regressions.md), `import-gaps`.

## Layout

| Path | Contents |
|---|---|
| `crates/orrery` | The server, `orrery`, and the database import, `orrery-import` |
| `crates/orrery-proto` | Shared wire formats: Klippy socket framing and JSON-RPC envelopes |
| `crates/fake-klippy` | A scriptable stand-in for Klipper's API socket |
| `crates/conformance` | The conformance test runner |
| `conformance/` | Test cases, fixtures, and recorded snapshots; see [conformance/README.md](conformance/README.md) |
| `docs/regressions.md` | Known differences from Moonraker |
| `docs/oddities.md` | Moonraker behaviour that looks odd but that orrery keeps on purpose |
| `docs/design/` | The plan, its decision log, and design notes such as the datastore spike |
| `spikes/` | Throwaway experiments, each a standalone crate outside the workspace |

## Development setup

The development tools come from a nix flake. With direnv, they load when you enter the directory.
Without direnv, run `nix develop`.

To set up the reference Moonraker and Klipper, then run everything:

```
just ref-setup
just klipper-setup
just
```

By default the recipes copy Moonraker and Klipper from `~/sources/moonraker` and `~/sources/klipper`.
Set `MOONRAKER_SRC` or `KLIPPER_SRC` to use other checkouts. The originals are never modified.

To build release binaries for the Pi:

```
just build-arm
```

The binaries link with `rust-lld` and Rust's bundled musl, so they need no C cross toolchain.

To build orrery for a printer's CPU and copy it to the printer over SSH, run:

```
scripts/copy-to-printer.sh <ssh host>
```

`<ssh host>` is what you'd give `ssh`, for example `pi@printer-1`. The binary goes to
`~/orrery-test/bin/orrery` on the printer, and it's copied only when it changed. The script
doesn't start orrery or change anything else on the printer. To close the SSH connection it keeps
open for 10 minutes, run `scripts/copy-to-printer.sh --disconnect <ssh host>`.
