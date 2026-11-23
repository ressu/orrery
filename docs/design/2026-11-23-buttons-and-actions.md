# Buttons and actions

**Decision (2026-11-23):** two pieces.

- **Actions:** named steps in `orrery.toml`, each one a method call or a gcode script. Orrery's
  own actions are `[action.<name>]`; an agent's are in its table, `[agent.<agent>.action.<name>]`
  (user, 2026-11-23). Anything that reacts to an event refers to actions by name. Buttons are
  the first; others can follow, such as job events or LED triggers, and new kinds of action,
  such as running a command (user, 2026-11-23).
- **Buttons:** an agent, `orrery-gpio`, that watches host GPIO lines and runs actions when a
  button is pressed, released, clicked, or held. It replaces Moonraker's `[button]`, without
  Jinja templates.

**Status (2026-11-23):** design. Nothing is built.

## Why

pink-0 has a Moonraker button, a GPIO line that shuts the machine down:

```ini
[button SKSM]
type: gpio
pin: !gpio72
on_press:
  {% do call_method("machine.shutdown") %}
```

Orrery has no button support, so the button does nothing on pink-0 today.

Moonraker's buttons run Jinja templates. A template can use `call_method(name, **kwargs)`,
`send_notification(data)`, the `event` (`pressed`, `elapsed_time`, `received_time`,
`render_time`), and `user_data`, a dict kept until Moonraker restarts. The output of the
template is thrown away (`components/button.py:115`). Gcode goes through
`call_method("printer.gcode.script", script=…)`. Moonraker's other templates don't call methods:
they fill in `secrets` once at startup, or turn data into text, such as a notification's title.

### What buttons do in practice

Moonraker's docs show a long press that reboots and a double click that keeps state. Neither is
common. The user collected the `[button]` sections from nine printers' config backups on GitHub,
pink-0 included: 13 handlers.

| Pattern | Handlers |
|---|---|
| One call | 7: emergency stop ×2, toggle a power device ×3, `printer.firmware_restart`, `machine.shutdown` |
| Several calls in a row | 1: two power devices on |
| A long press, as `elapsed_time` in `on_release` | 2: toggle a power device, reboot |
| `elapsed_time` in `on_press` | 1: shuts down after the button was *released* for 5 s, likely a mistake |
| Branches on state | 2: a macro if Klippy is ready, else power on; a power-loss handler |

`machine.device_power.post_device` is called 8 times, more than all other methods together.

Two more things the sample shows:

- **`elapsed_time` is easy to get wrong.** One button runs `printer.firmware_restart` on press and
  reboots on a long release, so a long press does both. Another tests `elapsed_time` on press,
  where it measures how long the button was up.
- **The power-loss handler belongs in Klipper.** It queries the print state, stops Klipper with
  an emergency stop, and then sends `SAVE_VARIABLE` gcode, which a shut-down Klipper refuses.
  A Klipper macro has the print state already. The button then runs the macro and shuts down:
  two actions.

So a list of actions per event, and a click and a long press as events of their own, cover 11
of the 13 handlers as written, and the power-loss handler once its logic is a macro. Only "a
macro if Klippy is ready, else power on" needs a condition.

## Actions

An action is one step. Its table says what to do:

```toml
[action.save_state]
gcode = "POWER_LOSS_SAVE"

[action.lights_off]
method = "printer.gcode.script"
params = { script = "SET_LED LED=case WHITE=0" }

# An action of the agent "power": its method is one of the agent's own.
[agent.power.action.printer_toggle]
method = "post_device"
params = { device = "printer", action = "toggle" }
```

| Option | What it does |
|---|---|
| `method` | A JSON-RPC method to call: one of Orrery's, such as `machine.shutdown`, or in an agent's table, one of the agent's, such as `post_device` |
| `params` | The method's arguments, a table. Default: none |
| `gcode` | A gcode script, sent with `printer.gcode.script`. Several lines run in order |
| `role` | The role a caller needs to run the action: `viewer`, `operator`, or `admin`. Default: `operator` |

An action has `method` or `gcode`, not both. Later kinds add an option of their own, such as
`command` to run a program (see [Later](#later)).

`gcode` is a shorthand: `printer.gcode.script` with `params = { script = … }` does the same. It's
there because running a macro is the common case.

### Names

An action's full name is the path of its table (user, 2026-11-23), so a name copied from the
config always works:

| Action | Full name | Short name |
|---|---|---|
| Orrery's, `[action.save_state]` | `action.save_state` | `save_state` |
| An agent's, `[agent.power.action.printer_toggle]` | `agent.power.action.printer_toggle` | `power.printer_toggle` |

Both forms work everywhere an action is named. The docs use the short names. Agent names can't
contain a dot, and an agent can't be named `action` or `agent`, so every name reads only one way.

### Checking actions

A method that doesn't exist should be found when Orrery starts, not when someone presses the
button. Where the method lives decides when it can be checked:

- **Orrery's actions** may call only methods Orrery answers itself. Orrery checks each `method`
  against its own methods at startup. An unknown one, or a name that a map points at an agent,
  is a config error in `server.info`, and the action is left out. A mapped name gets a hint to
  declare the action in the agent's table.
- **An agent's actions** live in its table, so only a configured agent, `[agent.<name>]` or a
  built-in one, has actions (user, 2026-11-23). An ad hoc agent has none: actions shouldn't
  depend on a program that may never connect. Orrery checks each `method` when the agent has
  registered its endpoints, and lists a missing one as a warning in `server.info` while the
  agent is connected. The action stays: the next version of the agent may have it.
- **An unknown action name** in a button's list is a config error, reported at startup, naming
  the button and the action.

Agent actions let an agent offer things to do without a public method name. A power agent's
`post_device` needs no map to `machine.device_power.post_device` to be used from a button, and
`orrery-gpio` could later offer outputs the same way, with `set_high` and `set_low` actions.

### Built-in actions

Orrery has these actions without any config (user, 2026-11-23):

| Action | Does |
|---|---|
| `shutdown` | `machine.shutdown` |
| `reboot` | `machine.reboot` |
| `emergency_stop` | `printer.emergency_stop` |
| `job_queue_pause` | `server.job_queue.pause` |
| `job_queue_start` | `server.job_queue.start` |

Each has the default role, `operator`. A table of the same name in `orrery.toml`, such as
`[action.shutdown]`, replaces the built-in one, so an admin can change its role or what it does.
A simple button then needs no `[action]` table:

```toml
[button.sksm]
pin = "!gpio72"
on_press = ["shutdown"]
```

The built-in Spoolman agent has one too, `spoolman.set_active_spool`: its `post_spool_id`, with
the caller's `spool_id`.

### Running an action

Orrery runs actions, not the agent or client that asks. Each request runs one action (user,
2026-11-23):

- **Clients and agents** call `server.actions.run {action: "save_state"}`, which returns the
  method's result when the action is done, or its error.
- **Klipper** calls the remote method `run_action`, named the way Moonraker names its remote
  methods (user, 2026-11-23):

  ```
  [gcode_macro SET_ACTIVE_SPOOL]
  gcode:
    {% set id = params.ID %}
    {action_call_remote_method("run_action", action="spoolman.set_active_spool", spool_id=id)}
  ```

**Arguments from the caller** go to the method, beside the action's own `params`, as `spool_id`
does above. The action's `params` are fixed: a caller's argument with the same name is refused,
so an action always does what its table says. A `gcode` action takes no arguments.

**Arguments keep their JSON types,** and the method converts them, as it does for any client.
Klipper sends a macro's values as they are: `params.ID` is text, `"3"`. Moonraker's methods
convert their arguments (`get_int` and the others in `common.py`), so `"3"` is spool 3, and
Orrery's methods do the same, as `post_spool_id` does today (`to_int` in
`crates/orrery-spoolman/src/main.rs`). Moonraker's remote method `spoolman_set_active_spool` is
the exception: it asserts an integer (`components/spoolman.py:283`), so its docs' macro converts
with `params.ID|int`. Run as an action, that remote method reaches `post_spool_id` and accepts
both, so every macro that worked with Moonraker still works, and `spool_id=params.ID` does too.
Actions add no conversion of their own.

- **One implementation,** so every feature that runs actions handles failures, logs, and later
  kinds of action the same way.
- **An agent's action calls that agent,** through Orrery's normal routing, so a button can run
  a power agent's action. The agent that asks doesn't need to know which agent an action
  belongs to.
- **Agents get no action tables** in their config, only the names their own settings list.
- **A button's list** runs its actions one after another, and stops at the first that fails.
  Orrery logs the failure with the action's name and the error, such as "Action
  power.printer_toggle failed: Agent power not connected". A failure isn't a `server.info`
  warning: a button pressed while the printer is off shouldn't leave a warning behind. Lists
  exist only on buttons; several steps from Klipper are several calls in a macro.

### Roles

Two checks, both on the caller's role (user, 2026-11-23):

1. **The action's `role`:** who may run it. The default is `operator`, so a viewer can't run
   actions. An admin raises it for an action that should be theirs alone, such as
   `role = "admin"` on a reboot.
2. **Each call's own role, as if the caller had sent it.** An action never does more than its
   caller could do directly. A button's list runs in order and stops at the first action that
   fails or is refused. Checking a whole list first would need each call's role up front,
   which an agent's methods don't declare, so a refused action can leave a list half done: put
   the action most likely to be refused first.

So the built-in `shutdown` can be run by an operator, but its call to `machine.shutdown` needs
`admin`. A button agent that Orrery started on the unix socket runs as Orrery's user, so it's
`admin`, and pink-0's button works. An operator in Fluidd gets "Only an admin can shut down the
printer's computer."

**Klipper runs actions as `operator`** (user, 2026-11-23). Its calls come from Klipper's
connection, which can't tell who sent the gcode, so they get `operator`: the role any client
needs to run a macro. Actions with `role = "admin"`, and calls that need `admin`, are refused
from Klipper. This closes, for actions, the gap in docs/design/2026-11-20-authorization.md
("Gcode can reach an agent past `call_role`"): a macro can't reach more than an operator could.
The one exception is Moonraker's `shutdown_machine` and `reboot_machine`, below.

### Moonraker's remote methods

Macros written for Moonraker call its remote methods by name. `[moonraker_compat]` registers
them with Klipper, each running an action, so the macros keep working (user, 2026-11-23):

| Remote method | Runs |
|---|---|
| `shutdown_machine` | `shutdown` |
| `reboot_machine` | `reboot` |
| `spoolman_set_active_spool` | `spoolman.set_active_spool`, with `spool_id` |
| `pause_job_queue` | `job_queue_pause` |
| `start_job_queue` | `job_queue_start` |

A remote method is registered only while its action exists, so `spoolman_set_active_spool`
comes with the Spoolman agent. Today that agent registers it itself
(`crates/orrery-spoolman/src/main.rs:176`); this moves it to Orrery. Moonraker's other remote
methods, `set_device_power`, `notify`, `publish_mqtt_topic`, `set_wled`, `set_wled_state`, and
`paneldue_beep`, belong to features Orrery doesn't have. `set_device_power` comes with a power
agent.

In Moonraker, any macro can shut down or reboot the machine. Auto-off macros do that at the end
of a print, so those two need `admin` from Klipper to keep working:

| Option | Default | What it does |
|---|---|---|
| `remote_methods` | `true` | Registers Moonraker's remote methods with Klipper |
| `klipper_shutdown` | `true` | Runs `shutdown_machine` and `reboot_machine` from Klipper as `admin`, as Moonraker does |

With `klipper_shutdown = false`, the two are refused like any other admin call from Klipper.
`run_action` with `action = "shutdown"` is always refused from Klipper: only the compatibility
names get `admin`, so a new macro can't reach it by accident.

## Buttons

```toml
[button.power]
pin = "^gpiochip0/gpio23"
on_click = ["power.printer_toggle"]
on_long_press = ["save_state", "shutdown"]
long_press_time = 2.0
```

| Option | Default | What it does |
|---|---|---|
| `pin` | required | The GPIO line, in Moonraker's syntax: `^` pull-up or `~` pull-down, `!` inverted, an optional `gpiochipN/` (default `gpiochip0`), then `gpioN` |
| `debounce_period` | `0.05` | Seconds the line must stay put before a change counts. At least `0.01` |
| `minimum_event_time` | `0` | Seconds: a change sooner than this after the previous one is ignored, as in Moonraker |
| `on_press` | none | Actions to run when the button goes down |
| `on_release` | none | Actions to run when the button comes up |
| `on_click` | none | Actions to run when the button comes up before `long_press_time` |
| `on_long_press` | none | Actions to run once the button has been held for `long_press_time`, while it's still held |
| `long_press_time` | `1.0` | Seconds that make a press long |

A button needs at least one of the four lists.

**Click and long press** replace `elapsed_time` tests. A press is one or the other, never both.
`on_long_press` runs as soon as the hold reaches `long_press_time`, so the user knows when to let
go. A Moonraker template has to wait for the release.

**One event at a time per button,** as in Moonraker: the next event waits until the previous
list is done.

**Notifications:** each event that runs a list is sent as `notify_button_event`, with the
button's name and the event (`press`, `release`, `click`, or `long_press`). Moonraker sends its
notification only when a template calls `send_notification`; no client listens for it.

### The agent

`orrery-gpio` is a built-in agent named `gpio`, like `orrery-spoolman`: Orrery starts it from
its own folder when a button is set up, in either file, and gives it the buttons' settings with
`orrery.agent.config`. The section `button` names the buttons, and `button <name>` has one's
settings, as Orrery converted them: times as numbers, each event as a list of action names, and
names that don't resolve left out. The agent never sees moonraker.conf's templates. Its event
`button_event` is mapped to `notify_button_event`. The name leaves room for GPIO outputs later.

An argument named `action` can't be passed through `run_action` or `server.actions.run`, since
that's where the action's name goes. Set it in the action's `params` instead, as
`power.printer_toggle` does.

- **GPIO through the kernel's character device,** `/dev/gpiochipN`: a line request with edge
  events and the pin's bias, through ioctls from the `rustix` or `libc` crates, which are
  already in `Cargo.lock`. No C library.
- **Debounce and `minimum_event_time` are plain logic over edge times,** tested with a fake line
  that replays edges, without hardware.
- **A line it can't open** is a warning in `server.info`, with the button's name and the error,
  such as "Button sksm: can't open gpiochip0 line 72: permission denied". The other buttons keep
  working. Setting up access to `/dev/gpiochipN` waits (see [Open questions](#open-questions)).

### Is an agent worth it?

An agent keeps GPIO access, and its failures, out of Orrery, and it's a feature Orrery doesn't
need on most printers. But a button that waits for one edge shouldn't cost much memory or wake
the CPU.

**Decision (2026-11-23):** build it as an agent, and check the cost later. About 200 seconds of
CPU a day is acceptable (user, 2026-11-23).

The two agents on pink-0 (2026-11-23, after 23.7 hours):

| Process | RSS | Private | Binary's pages | Threads | CPU |
|---|---|---|---|---|---|
| `orrery` | 12.5 MB | 5.0 MB | 7.5 MB | 6 | 1132 s, 1.3% |
| `orrery-spoolman` | 5.1 MB | 0.6 MB | 4.4 MB | 1 | 200 s, 0.23% |
| `orrery-updater` | 5.7 MB | 0.7 MB | 5.0 MB | 1 | 180 s, 0.21% |

An agent's own memory is under 1 MB; the rest is its program's pages, which the kernel can drop
and read again. The CPU time is the question: an update manager that checks once a week
shouldn't use 180 seconds a day. Both agents use about the same, so the shared `orrery-agent`
runtime is the likely cause, and every agent pays it.

The check, for later:

1. Find what wakes an idle agent, with `strace -c` or `/proc/<pid>/schedstat` over a few minutes
   on pink-0. If it's the runtime, fix it there.
2. Measure a prototype `orrery-gpio` with one button: memory, threads, and wakeups, idle and
   while pressed. Compare it with the same line read by a task inside Orrery.
3. **An agent** if its private memory stays under about 1 MB and it doesn't wake without an
   edge. Otherwise GPIO goes into Orrery, and the actions don't change: they run in Orrery
   either way.

## From moonraker.conf

The conversion (docs/design/2026-11-22-config-upgrade.md, "One way in") turns `[button <name>]`
sections into `[button.<name>]` tables. `pin`, `debounce_period`, and `minimum_event_time` are
copied.

**Only the simplest template converts** (user, 2026-11-23): one call, to one of Orrery's
methods, with literal arguments:

```
{% do call_method("<method>") %}
{% do call_method("<method>", key="text", key=123, key=True) %}
```

Anything more would need a Jinja parser, and a conversion that guesses wrong runs the wrong
thing when someone presses the button. So the conversion matches this one shape exactly, with
whitespace and comment lines around it allowed. The arguments may be quoted text, numbers, and
`True` or `False`. The template becomes an action named after the button and the option,
`sksm_on_press`, with the default role, and the option keeps its name. A call that's exactly a
built-in action, `machine.shutdown` or `printer.emergency_stop` without arguments, uses the
built-in instead.

Everything else is left out, with a warning that names the button and the option: "Button
power: Orrery converts only a single call_method from moonraker.conf; set this button's actions in
orrery.toml". That covers several calls, any `{% if %}`, variables, and calls to agents, such as
`machine.device_power.*`. The button keeps the options that converted. A button left with none
is left out.

pink-0's button converts to:

```toml
[button.sksm]
pin = "!gpio72"
on_press = ["shutdown"]
```

**Regression entry** `button-templates` in docs/regressions.md: buttons run actions, not
templates; what converts; `notify_button_event`'s payload; Klipper's calls run as `operator`,
except `shutdown_machine` and `reboot_machine`.

## Later

**Commands:** `command = ["~/bin/lights", "on"]` in an action runs a program, as Orrery's user,
with the action's event in environment variables. It covers what a method can't, in any
language, with nothing embedded in Orrery.

**Conditions:** for "a macro if Klippy is ready, else power on". Two ways, decided when a real
need comes:

- A condition on an action, such as `when = "klippy_ready"`, from a fixed list Orrery knows.
- A script action, `script = '…'`, in an embedded language.

The language spike (`spikes/script-languages/README.md`, 2026-11-23) measured candidates on
aarch64. Rhai with `Engine::new_raw()` adds 1.18 MB to a binary and stops runaway loops by
counting operations. Koto adds 1.86 MB, Rune 5.12 MB. evalexpr is ruled out: its `if` is a
function that evaluates both branches, so a long press would reboot on every release. The user
prefers Rhai's syntax to Koto's. A script would run in Orrery, as actions do, so the size lands
in one binary.

**Other triggers:** job events (started, finished, cancelled), LED triggers, the job queue's
transition. Each names its actions in the feature's own table; whether it takes a list, as buttons do, is decided with it.

## Not in this change

- Power devices (`machine.device_power.*`). Most buttons in the sample toggle one, so a power
  agent is the natural next piece. Its actions would live in its table.
- GPIO outputs, later as `orrery-gpio`'s own actions.
- `user_data`, `send_notification` with data, and Moonraker's `elapsed_time`.

## Open questions

1. **Access to `/dev/gpiochipN`.** On pink-0 the chips are `root:gpio`, mode `0660`, and
   `printer` is in `gpio`, but the udev rule that does it may be the user's own, not Armbian's.
   What other images ship, and whether `orrery setup` should set it up, is left for later
   (user, 2026-11-23).
2. **More built-in actions,** such as `firmware_restart`. They're cheap to add; each one waits
   for a use.
3. **Listing actions for clients,** so a frontend could show them as buttons. Fluidd and Mainsail
   don't know about actions, so this waits for a client that would use it.
4. **The client method's name,** `server.actions.run`. Moonraker might add a `server.actions`
   namespace of its own one day.
