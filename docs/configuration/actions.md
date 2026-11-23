# Actions

An action is a named step that a button, a Klipper macro, or a client can run: a call to one
of Orrery's methods, a call to an agent's method, or gcode.

```toml
[action.save_state]
gcode = "SAVE_VARIABLE VARIABLE=power_loss VALUE=1"

[action.lights_off]
method = "printer.gcode.script"
params = { script = "SET_PIN PIN=lights VALUE=0" }

[agent.power.action.printer_toggle]
method = "post_device"
params = { device = "printer", action = "toggle" }
```

| Option | Default | What it does |
|---|---|---|
| `method` | none | The method to call. In an agent's table, one of that agent's own methods, such as `post_device` |
| `params` | none | The method's arguments |
| `gcode` | none | Gcode to run, instead of `method` |
| `role` | `operator` | The role a caller needs to run the action: `viewer`, `operator`, or `admin` |

An action has `method` or `gcode`, not both.

## Names

`[action.<name>]` is one of Orrery's actions, and `[agent.<agent>.action.<name>]` is an agent's.
Call them by the short names, `save_state` and `power.printer_toggle`. The table's path, such
as `action.save_state` or `agent.power.action.printer_toggle`, works too.

An agent's actions go in its own table, so an agent needs an `[agent.<name>]` table to have
any. Orrery checks its own actions when it starts, and leaves out one whose method it doesn't
have, with a warning. It checks an agent's actions when the agent connects.

## Built-in actions

| Action | What it calls |
|---|---|
| `shutdown` | `machine.shutdown` |
| `reboot` | `machine.reboot` |
| `emergency_stop` | `printer.emergency_stop` |
| `job_queue_pause` | `server.job_queue.pause` |
| `job_queue_start` | `server.job_queue.start` |
| `spoolman.set_active_spool` | The Spoolman agent's `post_spool_id`, when [Spoolman](spoolman.md) is on |

A table of the same name replaces a built-in action. For example, to let only admins reboot:

```toml
[action.reboot]
method = "machine.reboot"
role = "admin"
```

## Roles

Running an action takes two checks:

1. The caller needs the action's `role`.
2. Each call the action makes is checked as if the caller had sent it. An operator can run
   `shutdown`, but `machine.shutdown` needs an admin, so the operator is refused.

Klipper's macros run actions as an operator, since Orrery can't tell who sent the gcode.

## Running an action

A client or an agent calls `server.actions.run`, with the action's name and the method's
arguments:

```json
{"method": "server.actions.run", "params": {"action": "spoolman.set_active_spool", "spool_id": 3}}
```

A Klipper macro calls the remote method `run_action`:

```
[gcode_macro SET_ACTIVE_SPOOL]
gcode:
  {% set id = params.ID %}
  {action_call_remote_method("run_action", action="spoolman.set_active_spool", spool_id=id)}
```

The caller's arguments go to the method with the action's `params`. A caller can't change an
argument the action's `params` set, and an action that runs gcode takes no arguments. The
method converts the arguments as it would from any client, so `spool_id` can be `"3"` or `3`.

## Moonraker's remote methods

Macros written for Moonraker call its remote methods, such as
`action_call_remote_method("shutdown_machine")`. Orrery offers them as actions:

| Remote method | Action |
|---|---|
| `shutdown_machine` | `shutdown` |
| `reboot_machine` | `reboot` |
| `spoolman_set_active_spool` | `spoolman.set_active_spool` |
| `pause_job_queue` | `job_queue_pause` |
| `start_job_queue` | `job_queue_start` |

Moonraker lets any macro shut down or reboot the printer's computer, so Orrery runs
`shutdown_machine` and `reboot_machine` as an admin. `run_action` with `shutdown` doesn't. Two
options in [`[moonraker_compat]`](access.md#moonraker-compatibility) turn these off.
