# Agents

An agent is a program that adds features to Orrery. It connects to Orrery's unix socket,
`~/printer_data/comms/moonraker.sock`, and offers methods that clients can call. Orrery's
Spoolman support and its update manager are agents.

Each agent has a table in `orrery.toml`, `[agent.<name>]`. `<name>` may use letters, digits,
`_`, and `-`. A configured agent connects over the unix socket, or from anywhere with its API key.

To write an agent, see [the plugin interface](../design/2026-11-15-plugin-interface.md). For an
agent in Rust, use the `orrery-agent` crate.

## Starting an agent

Orrery can start the agent, systemd can, or you can.

### Orrery starts it

Name the program in `command`:

```toml
[agent.hello]
command = ["python3", "/home/pi/hello_agent.py"]
```

`command` is the program, or a list of the program and its arguments. Orrery runs it in the
data folder, `~/printer_data`, and sets two environment variables:

- `ORRERY_SOCKET`: the path of the unix socket.
- `ORRERY_AGENT`: the agent's name.

The agent's output goes to Orrery's log. If the agent exits, Orrery starts it again, after a
wait that grows from 1 second to a minute. When Orrery stops, so does the agent.

The agent must connect from that process, or from one it started, such as the program a
wrapper script runs.

### systemd starts it

To run the agent as a systemd service, name its unit:

```toml
[agent.spoolman]
systemd_unit = "orrery-spoolman.service"
systemd_scope = "user"
```

`systemd_scope` is `system`, the default, or `user`, for a unit of Orrery's user in
`~/.config/systemd/user`. A user unit has to run while nobody is logged in, which needs lingering. `sudo orrery setup`
turns it on for Orrery's user.

With a unit:

- Orrery starts the unit if it isn't running, and leaves it running when Orrery stops.
- The unit's own `Restart=` setting starts the agent again if it exits.
- Only a process in that unit can connect as the agent.
- Fluidd's service list shows the unit, with its restart button, so you can restart the agent
  alone. A user unit is listed as `user/<unit>`, such as `user/orrery-spoolman`.

An agent has `command` or `systemd_unit`, not both.

### You start it

Leave out `command` and `systemd_unit`. For a built-in agent, such as Spoolman, set
`command = false`. Then start the agent after Orrery, as the same user:

```
orrery-spoolman ~/printer_data/comms/moonraker.sock
```

Without the path, Orrery's agents use `$ORRERY_SOCKET`, then
`~/printer_data/comms/moonraker.sock`. They connect again by themselves when Orrery restarts.

## Options

| Option | Default | What it does |
|---|---|---|
| `command` | none | The program Orrery starts, a list of the program and its arguments, or `false` |
| `systemd_unit` | none | The systemd unit that runs the agent |
| `systemd_scope` | `system` | `system` or `user`: where `systemd_unit` is |
| `user` | none | A Unix user name or id. Only that user's processes can connect as the agent |
| `role` | `agent` | What the agent may do itself: `agent`, `operator`, or `admin`. `agent` is `viewer`, plus the agent's own methods |
| `call_role` | `operator` | The role a client needs to call the agent's methods |
| `call_roles` | none | A role for each method that needs another one, such as `status = "viewer"` |
| `component` | none | A name that `server.info` lists among Moonraker's components |
| `history_fields` | none | Fields the agent adds to each job in the print history |
| `maps` | none | Moonraker's method and notification names that the agent answers to |

## Answering under Moonraker's names

Clients call an agent's methods as `agent.<name>.<method>`. Fluidd and Mainsail don't know these
names, so `maps` lets an agent answer under Moonraker's. This example gives a Spoolman agent
Fluidd's Spoolman methods and notifications:

```toml
[agent.spoolman]
component = "spoolman"
history_fields = ["spool_ids"]

[agent.spoolman.maps]
"server.spoolman" = "agent.spoolman"
"notify_active_spool_set" = "agent.spoolman.active_spool_set"
"notify_spoolman_status_changed" = "agent.spoolman.spoolman_status_changed"
```

- A method name maps to the agent's own methods. `"server.spoolman" = "agent.spoolman"` makes
  `server.spoolman.get_spool_id` call `agent.spoolman.get_spool_id`.
- A name that starts with `notify_` maps to one of the agent's events. Orrery sends the event to
  clients as that Moonraker notification.
- Names that start with `agent.` or `orrery.` can't be mapped.

## Built-in agents

Orrery sets up two agents by itself, when their sections are in the config:

| Agent | Turned on by | Page |
|---|---|---|
| `spoolman` | `[spoolman]` with a `server` | [Spoolman](spoolman.md) |
| `update_manager` | `[update_manager]`, or any `[update_manager <name>]` | [The update manager](update-manager.md) |

Orrery starts them from the folder its own program is in, such as `~/orrery`. An
`[agent.spoolman]` or `[agent.update_manager]` table changes their setup one option at a time.
Options you leave out keep the built-in setup.
