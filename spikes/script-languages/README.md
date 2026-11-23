# Script language spike

Which embedded language should run button actions? Each binary runs Moonraker's button examples
(`src/common.rs`): an emergency stop on press, a reboot on a long press, a double click that keeps
state, a gcode script, and a runaway loop that the language's limit should stop.

```
cargo run --features <name> --bin <name>
```

## Results (2026-11-23)

Release builds for `aarch64-unknown-linux-musl`, stripped, thin LTO, each from a clean target
folder. "Added" is the size over `baseline`, the same program without a language (418 KB). For
scale, orrery's own aarch64 binary is 9.9 MB, and orrery-spoolman is 5.9 MB.

| Language | Added size | Clean build | Crates | All scenarios | Runaway stopped by |
|---|---|---|---|---|---|
| evalexpr 14 | 0.26 MB | 4 s | 7 | **No:** `if` is a function, so both branches run; the long press reboots on every release | nothing to stop: no loops |
| minijinja 3 (reference) | 1.09 MB | 8 s | 8 | No double click: Moonraker's `user_data.__setitem__` is Python | no unbounded loops |
| Rhai 1.27, `Engine::new_raw()` | 1.18 MB | 18 s | 32 | Yes | operation count |
| Koto 0.16 | 1.86 MB | 9 s | 41 | Yes | time limit |
| Rhai 1.27, `Engine::new()` | 2.38 MB | 18 s | 32 | Yes | operation count |
| Rune 0.14 | 5.12 MB | 45 s | 47 | Yes | instruction budget |

Rhai is built with `only_i64`, `no_module`, `no_custom_syntax`, `no_time`, and `serde`.
`new_raw()` leaves out the standard packages (string, array, and math functions); operators,
maps, `if`, `??`, and loops still work.

## The long press, in each

```
evalexpr   if(event_elapsed_time > 1.0, call_method("machine.reboot"), ())   # runs both
minijinja  {% if event.elapsed_time > 1.0 %}{% do call_method("machine.reboot") %}{% endif %}
rhai       if event.elapsed_time > 1.0 { call_method("machine.reboot") }
koto       if event.elapsed_time > 1.0 then call_method 'machine.reboot'
rune       if event.elapsed_time > 1.0 { call_method("machine.reboot", #{}); }
```
