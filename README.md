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
`server.extensions.*`, the agent methods (`connection.send_event` and
`connection.register_remote_method`),
`machine.system_info`, and `machine.proc_stats` APIs, with file upload and download. The same
JSON-RPC API is also on a unix socket, `comms/moonraker.sock`, for local tools and agents.
Clients get rights from trusted networks and API keys, each with a role: `viewer`, `operator`,
or `admin`. With a `moonraker.conf`, trusted networks and Fluidd's API key work as in
Moonraker. User logins aren't written yet. See
[docs/design/2026-11-20-authorization.md](docs/design/2026-11-20-authorization.md) and
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

## Configuration

orrery reads `moonraker.conf`, so a Moonraker setup works as it is. orrery's own settings go in
`orrery.toml`, next to `moonraker.conf` in the config folder. The file is optional, and when
both files set an option, `orrery.toml` wins. Moonraker never reads `orrery.toml`, so switching
back to Moonraker stays clean. `--orrery-config <path>` reads it from somewhere else.

`orrery.toml` uses Moonraker's section and option names. A section with a name, such as
`[webcam front]`, is a table inside a table:

```toml
[server]
port = 7125

[webcam.front]
service = "mjpegstreamer"
stream_url = "/webcam/?action=stream"
```

Values have TOML types, so `port = "7125"` is an error: orrery expects a number there.

An `[agent.<name>]` table lets an agent answer under Moonraker's built-in names. For example,
this gives a Spoolman agent Fluidd's Spoolman methods and notifications, lists `spoolman` in
`server.info`, and lets it add the spools used to each job's history:

```toml
[agent.spoolman]
component = "spoolman"
history_fields = ["spool_ids"]

[agent.spoolman.maps]
"server.spoolman" = "agent.spoolman"
"notify_active_spool_set" = "agent.spoolman.active_spool_set"
"notify_spoolman_status_changed" = "agent.spoolman.spoolman_status_changed"
```

A configured agent can connect only over the unix socket. `user = "printer"` also limits it to
one Unix user. An agent that orrery starts with `command` must connect from that process, or
from one it started, such as the program a wrapper script runs. See [docs/design/2026-11-15-plugin-interface.md](docs/design/2026-11-15-plugin-interface.md)
for how agents work. To write an agent in Rust, use the `orrery-agent` crate.

### Spoolman

Moonraker's Spoolman integration is a separate program in orrery, `orrery-spoolman`. It reads
the same `[spoolman]` section, from `moonraker.conf` or `orrery.toml`, so a migrated printer
needs no other setup:

- Without a `server` in `[spoolman]`, Spoolman is off.
- With one, orrery starts `orrery-spoolman` from its own folder, and Fluidd and Mainsail reach it
  under Moonraker's names. If the program isn't there, orrery says so in a warning.

The agent takes the active spool from Moonraker's database on its first run.

An `[agent.spoolman]` table in `orrery.toml` changes the built-in setup one option at a time.
For example, `command = "~/bin/orrery-spoolman"` runs the agent from another place, and
`command = false` means you start it yourself.

### Starting agents

`command` in an agent's table is the program orrery starts, or a list of the program and its
arguments, such as `["python3", "my_agent.py"]`. orrery runs it in its data folder
(`~/printer_data`), with the unix socket's path in `ORRERY_SOCKET` and the agent's name in
`ORRERY_AGENT`. The agent's output goes to orrery's log. If it exits, orrery starts it again,
after a wait that grows from 1 second to a minute. When orrery stops, so does the agent.

To start an agent yourself instead, leave out `command`, or set it to `false` for a built-in
agent. Run it after orrery, as the same user:

```
orrery-spoolman ~/printer_data/comms/moonraker.sock
```

Without the path, it uses `$ORRERY_SOCKET`, then `~/printer_data/comms/moonraker.sock`. It
connects again by itself when orrery restarts.

To run an agent as a systemd service, name its unit in the agent's table:

```toml
[agent.spoolman]
systemd_unit = "orrery-spoolman.service"
systemd_scope = "user"
```

`systemd_scope` is `system`, the default, or `user` for a unit of orrery's user, in
`~/.config/systemd/user`. A user unit needs that user's systemd running while nobody is logged
in, which `loginctl enable-linger <user>` turns on. With a unit:

- orrery starts the unit if it isn't running, and leaves it running when orrery stops. The unit's
  own `Restart=` setting starts the agent again if it exits.
- Only a process in that unit can connect under the agent's name.
- The unit appears in Fluidd's service list, with its restart button, so you can restart the
  agent alone. A user unit is listed as `user/<unit>`, such as `user/orrery-spoolman`.
- An agent has `command` or `systemd_unit`, not both. With `systemd_unit`, orrery doesn't start
  the built-in Spoolman program itself.

## Layout

| Path | Contents |
|---|---|
| `crates/orrery` | The server, `orrery`, and the database import, `orrery-import` |
| `crates/orrery-agent` | A client for writing agents in Rust; see its `examples/hello.rs` |
| `crates/orrery-spoolman` | Moonraker's Spoolman integration, as an agent |
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

To build orrery, `orrery-import`, and `orrery-spoolman` for a printer's CPU and copy them to the
printer over SSH, run:

```
scripts/copy-to-printer.sh <ssh host>
```

`<ssh host>` is what you'd give `ssh`, for example `pi@printer-1`. The programs go to
`~/orrery-test/bin` on the printer, and each is copied only when it changed. The script
doesn't start orrery or change anything else on the printer. To close the SSH connection it keeps
open for 10 minutes, run `scripts/copy-to-printer.sh --disconnect <ssh host>`.

To run orrery on the printer as a temporary systemd unit, `orrery-test`, run:

```
just printer-test start <ssh host>
```

It stops Moonraker, and an installed `orrery.service`, while it runs. The unit runs as your user
with the `moonraker-admin` group, so orrery can manage services through Moonraker's polkit rule.
No unit file is written. `just printer-test log <ssh host>` follows the log, and
`just printer-test stop <ssh host>` stops the unit and starts again what it stopped. Options after
the host go to orrery, for example `just printer-test start <ssh host> --verbose`.

To keep one SSH connection open while you work, so you log in once, run:

```
just connect-printer <ssh host>
```

Later `ssh`, `scp`, and `copy-to-printer` runs can share it. It closes after 12 hours unused;
`just connect-printer <ssh host> 1h` sets another time. To close it, run:

```
just disconnect-printer <ssh host>
```
