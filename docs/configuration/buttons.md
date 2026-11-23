# Buttons

A button on one of the printer computer's own GPIO lines runs [actions](actions.md) when it's
pressed, released, clicked, or held.

```toml
[button.power]
pin = "^gpiochip0/gpio23"
on_click = ["power.printer_toggle"]
on_long_press = ["save_state", "shutdown"]
long_press_time = 2.0
```

| Option | Default | What it does |
|---|---|---|
| `pin` | required | The GPIO line, as Moonraker writes it: `^` for a pull-up or `~` for a pull-down, `!` to invert, an optional `gpiochipN/` (default `gpiochip0`), then `gpioN` |
| `debounce_period` | `0.05` | Seconds the line must stay put before a change counts. At least `0.01` |
| `minimum_event_time` | `0` | Seconds: a change sooner than this after the previous one is ignored |
| `on_press` | none | Actions to run when the button goes down |
| `on_release` | none | Actions to run when the button comes up |
| `on_click` | none | Actions to run when the button comes up before `long_press_time` |
| `on_long_press` | none | Actions to run once the button has been held for `long_press_time` |
| `long_press_time` | `1.0` | Seconds that make a press long |

A press is a click or a long press, never both. `on_long_press` runs while the button is still
held, so you know when to let go.

Each list runs in order, and stops at the first action that fails. A button runs one event at
a time: the next waits until the previous list is done. Each event that runs a list is sent to
clients as `notify_button_event`, with the button's name and the event: `press`, `release`,
`click`, or `long_press`.

## From moonraker.conf

Moonraker's buttons run Jinja templates. Orrery converts a template that's a single
`call_method` with fixed arguments:

```ini
[button SKSM]
pin: !gpio72
on_press:
  {% do call_method("machine.shutdown") %}
```

This button runs the built-in `shutdown` action. Any other template, such as one with
`{% if %}` or more than one call, is left out with a warning. Set that button's actions in
`orrery.toml` instead.

## How it runs

Orrery's buttons are a separate program, `orrery-gpio`, in the same folder as Orrery. Orrery
starts it when a button is set up. The program needs to read and write `/dev/gpiochipN`; a line
it can't open is a warning, and the other buttons keep working.

To change how it runs, add an `[agent.gpio]` table; see [Agents](agents.md).
