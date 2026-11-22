# LED controllers

**Status (2026-11-22):** discussed with the user, who answered the questions on config,
macros, and scope. The wire format and Moonraker's WLED API are left for later. Nothing is
built.

orrery gets an LED agent, `orrery-wled`, that drives WLED controllers. The whole LED setup
lives in orrery's config: the controllers, their LEDs, the effects, and which printer states
play them. When a controller connects, the agent sets it up to match the config. The controller
runs the animation itself, so neither orrery nor Klipper does any work per frame.

## Why

Today an LED setup on a Klipper printer takes one of two routes, and both hurt:

- **Moonraker's `[wled]`** can trigger presets, but the setup is spread over four places. You
  build and flash WLED, join it to the network, make the presets in WLED's own UI, add it to
  `moonraker.conf`, and then write the macros that call it. It works well for simple things,
  such as "chamber light on" or "blink red when the filament runs out", but you lose more than
  you need to.
- **led_effect** keeps everything in Klipper's config and reacts to the print as it runs, such
  as Pink-0's gradients that follow the bed and hotend temperatures. But Klipper renders every
  frame, at 24 frames a second, and sends it to the LEDs.

This design keeps led_effect's "one place to configure it" and its reaction to the printer's
state, and lets WLED do the animation it's already good at.

## What it isn't

**It isn't a replacement for led_effect** (user, 2026-11-22). It's a new feature that works the
way we want it to. So there's no importer for `[led_effect]` sections, and the config doesn't
copy led_effect's layer syntax. Importing would only tie the design to led_effect's model.

**What we give up:** effects that follow fast events. led_effect's `homing` layer reacts to
endstops from inside Klipper's homing code. The agent only sees `toolhead.homed_axes` change in
status updates, about 250 ms later. That's enough to flash once per axis, but it can't follow
the endstop as it triggers. The user accepts this.

## The pieces

| Piece | Where it runs | What it does |
|---|---|---|
| `orrery-wled` | The printer's host, as an agent | Reads the config, follows the printer's state, sets up controllers, and tells them what to play |
| WLED with an orrery usermod | Each LED controller | Plays effects, takes setup and commands over serial or CAN, and reports the hash of its setup |
| The LED config | orrery's config | Everything about the LEDs, in one place |

A **usermod** is WLED's way to add features: C++ code built into the firmware, without forking
WLED.

The agent works like the Spoolman agent: it subscribes to the printer objects it needs, and
registers a remote method so that macros can call it. orrery starts it with `command`, as it
starts `orrery-spoolman` (see `2026-11-15-plugin-interface.md`).

## Connecting a controller

A controller connects in one of three ways, chosen per controller. The agent sends the same
messages over each one.

- **IP:** WLED's JSON API, over HTTP or its websocket. This works with stock WLED, but it puts
  one more device on WiFi, and the user doesn't want to depend on WiFi on the printer.
- **Serial:** a USB cable, or a UART on the host's pins. Stock WLED already takes JSON state
  over serial at 115200 baud; Moonraker's `StripSerial` uses it.
- **CAN:** a bus that several controllers share. It needs the fewest parts, but the ESP32
  boards people have rarely have a CAN transceiver.

The first target is the boards people already own, so serial and IP come first. New boards
built for this are out of scope for this note.

## Setting up a controller from the config

The agent sets up a controller so that plugging in a new one is the only step. It works like
`orrery import`: run it any number of times, and the result is the same.

1. The controller connects, and reports a hash of the setup it holds.
2. The agent compares it with the hash of the config.
3. If they differ, the agent sends the setup: the LED count and color order, the segments, and
   the effects. Then the controller saves it and reports the new hash.

What stock WLED allows decides where the usermod is needed:

- **Over IP,** WLED's HTTP API can already write its settings (`/json/cfg`) and presets. This
  is still to check.
- **Over serial,** stock WLED takes only state: the effect, colors, speed, and so on. Writing
  settings and saving the setup needs the usermod.

**First boot.** WLED opens its own WiFi access point when it has no network to join. For a
controller on serial or CAN, the setup turns that access point off, or the config says to keep
it for debugging.

## Effects

Effects are WLED's own effects: breathe, gradient, chase, strobe, percent, and the rest. Where
none fits, the usermod adds a custom effect. Blending several layers, as led_effect does, isn't
a goal; it's one effect that we can build if we need it (user, 2026-11-22).

**Inputs.** Some effects follow a value from the printer, such as a heater's temperature
against its target, or the print's progress. The agent sends the value when it changes, at
most about twice a second, and the controller animates in between. These values change slowly,
so the effect should look the same as one Klipper renders.

**When the host stops.** The agent sends a heartbeat. If the controller stops hearing it, it
plays the config's "host lost" effect by itself. led_effect can't do this, because it stops
when Klipper stops.

## States

The config maps printer states to effects, so most of today's LED macros aren't needed.
Pink-0 has 16 `status_*` macros, and each one stops every effect and starts a list of new ones.
That's a table, and it belongs in the config.

- **States the agent can see** come from Klipper's status: idle, heating, printing, paused,
  done, and error.
- **States only a macro knows,** such as meshing or cleaning, come from one macro, `LED_STATE
  STATE=<name>`, which calls the agent's remote method. The agent publishes this macro, so
  nobody writes it by hand (see [Publishing the macros](#publishing-the-macros)).
- **Each state has a priority.** The highest active state plays. When a filament runout clears,
  the LEDs go back to the printing effect, with no macro having to remember it.

## Publishing the macros

The agent writes its macros into a file in the `config` root, through orrery's file upload
code (user, 2026-11-22). That's the code `server.files.upload` uses, so the file gets the same
checks, and Fluidd hears about it with `notify_filelist_changed`. Klipper and Kalico both read
it with one line in `printer.cfg`, such as `[include agents/wled/macros.cfg]`.

- **A new agent method** writes the file, because agents talk over the socket, not HTTP
  uploads. An agent may write only inside its own folder, such as `agents/wled/`, so it can't
  change the rest of the printer's config.
- **The agent writes only when the content changes,** so an unchanged file doesn't wear the SD
  card or announce a change.
- **Klipper loads macros when it restarts.** The agent never restarts Klipper, since a print
  might be running. It adds a warning that Klipper needs a restart, and Fluidd shows it.

## The config

**The LED config goes in `orrery.toml`** by default (user, 2026-11-22). The agent never reads a
file: it asks orrery for its config, so it doesn't need to know where the file is, and it works
the same when it runs on another machine.

Most of this exists already. `orrery.agent.config` returns an agent's own table as typed JSON,
and the tables nested under `[agent.wled]`, such as `[agent.wled.controller.case]`, come with
it. The Spoolman agent already uses it.

**A separate file for big setups** is the agent's own flag, `--config <file>`, not an option in
`orrery.toml` (user, 2026-11-22). The file is where the agent runs, and `orrery.toml` shouldn't
need to know about the agent's machine. With the flag, the agent reads its settings from that
file and doesn't ask orrery. A relative path is relative to the agent's working folder.

orrery needs nothing new for this. The plugin interface note already says agents can read
their settings from orrery's config or from their own file.

```
orrery-wled --config ~/printer_data/config/leds.toml
```

This sketch only shows the shape. The names and layout get decided when it's built.

```toml
[agent.wled]
command = "~/orrery/agents/orrery-wled"

[agent.wled.controller.case]
transport = "serial"
port = "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_XX:XX:XX:XX:XX:XX-if00"
leds = 20
color_order = "GRB"
segments = { left = [0, 10], right = [10, 20] }

[agent.wled.effect.heating]
"case.left" = { effect = "Percent", input = "heater_bed", colors = ["#00ffff", "#ff0000"] }
"case.right" = { effect = "Percent", input = "extruder", colors = ["#00ffff", "#ff0000"] }

[agent.wled.state]
error = { effect = "critical", priority = 100 }
meshing = { effect = "meshing", priority = 50 }
heating = { effect = "heating", priority = 40 }
idle = { effect = "standby", priority = 0 }
```

Effect names are WLED's. The agent asks each controller for its list of effects, and turns the
names into WLED's numbers, so a config doesn't break when a WLED version renumbers its effects.

## The order of work

1. **The agent with stock WLED,** over serial and IP. It has the states and their priorities,
   the published macro, inputs, and the whole config in one place. orrery gains the method
   that lets an agent write into its own config folder. On each change it sends the
   complete state. This already fixes the "four places" problem for anyone with stock WLED.
2. **The usermod:** setting up a controller from the config over serial, the heartbeat and
   "host lost" effect, and custom effects where WLED's don't fit.
3. **CAN,** for controllers with a transceiver.
4. **Flashing:** `espflash` is pure Rust, so the agent could flash a prebuilt image over USB the
   first time.

Step 1 is useful by itself, and it tests the shape of the config before any firmware work.

## Left for later

- **Moonraker's `[wled]` and `machine.wled.*`:** left open (user, 2026-11-22). They might come
  later as part of `[moonraker_compat]`, but this design puts no effort into them.
- **Messages on the wire:** designed with the final design (user, 2026-11-22). CAN frames hold
  8 bytes, so CAN needs a compact encoding, and serial might share it.
- **Config changes while the agent runs:** whether orrery tells the agent that its config
  changed, or the agent restarts.
