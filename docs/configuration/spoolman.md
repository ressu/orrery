# Spoolman

[Spoolman](https://github.com/Donkie/Spoolman) keeps track of your filament spools. Orrery
connects to it as Moonraker does, so Fluidd's and Mainsail's Spoolman panels work as before.

## Turning it on

Set the address of your Spoolman server in `[spoolman]`, in `moonraker.conf` or `orrery.toml`:

```toml
[spoolman]
server = "http://192.168.1.20:7912"
```

A printer that used Spoolman with Moonraker needs no other setup. Without `server`, Spoolman is
off.

| Option | Default | What it does |
|---|---|---|
| `server` | required | The address of the Spoolman server |
| `sync_rate` | `5` | How often Orrery reports the filament used, in seconds |

## How it runs

Orrery's Spoolman support is a separate program, `orrery-spoolman`, in the same folder as
Orrery. Orrery starts it when `[spoolman]` has a `server`. If the program isn't there, Orrery
says so in a warning, which Fluidd and Mainsail show.

`orrery-spoolman` starts without an active spool, even after an import. Choose the spool again
in Fluidd or Mainsail, or with `SET_ACTIVE_SPOOL`.

To change how it runs, add an `[agent.spoolman]` table. For example, to run the program from
another place:

```toml
[agent.spoolman]
command = "~/bin/orrery-spoolman"
```

With `command = false`, you start it yourself. To run it as a systemd service, see
[Agents](agents.md#systemd-starts-it).
