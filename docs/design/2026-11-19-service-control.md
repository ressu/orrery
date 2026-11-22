# Service control: D-Bus or systemctl

**Decision (2026-11-19):** orrery supports systemd first, and talks to it over D-Bus, the
system's message bus, as Moonraker's default `systemd_dbus` provider does. Other service
managers may come later, so service control sits behind one interface, and nothing outside it
may depend on systemd. The D-Bus client is a small one written for orrery, not the `zbus` crate.
This note compares the options and records what bluev2 has.

**Status (2026-11-20):** built, except the `systemd_cli` and `supervisord_cli` providers and
`orrery setup`. What differs from Moonraker is in `no-service-control` in `docs/regressions.md`.

Service control closes most of `no-service-control` in `docs/regressions.md`. It gives Fluidd's
service list and its restart buttons, `machine.reboot` and `machine.shutdown`, agents started
with `systemd_unit`, and Klipper's log rollover, which stops and starts Klipper.

## What orrery needs from systemd

Moonraker's `systemd_dbus` provider does these, and orrery needs the same:

- **List the services** that are loaded and allowed: Klipper, Moonraker, and the ones named in
  `moonraker.asvc`. Each comes with its `ActiveState` and `SubState`.
- **Follow their state.** Moonraker asks systemd to report changes to each service's properties,
  and sends `notify_service_state_changed` when one changes. It doesn't poll.
- **Start, stop, and restart** a service.
- **Reboot and shut down,** through `systemd-logind`.
- **Find a process's unit**, to fill `instance_ids`, and read the unit's properties, such as
  `ExecStart` and `FragmentPath`.
- **Check permissions at startup** with polkit, and add a warning for each action that isn't
  allowed.
- **Read the virtualization type.** orrery runs `systemd-detect-virt` for this today.

## The options

| | `sudo systemctl` | `systemctl` (no sudo) | D-Bus, `zbus` crate | D-Bus, own client |
|---|---|---|---|---|
| Permission | sudo for `printer` | polkit | polkit | polkit |
| Service state | polling | polling | change signals | change signals |
| Idle cost | about 2.7% of a core | about 2.7% of a core | none | none |
| Binary size (aarch64) | none | none | +1.27 to 1.56 MB | under 0.1 MB (estimate) |
| Code in orrery | small | small | small | about 700 to 900 lines |

**Polling is what makes the command-line options expensive.** Moonraker's `systemd_cli`
provider runs `systemctl show -p ActiveState,SubState` for every service, every 2 seconds. On
bluev2, one run for five services takes about 55 ms of CPU, so polling costs about 2.7% of a
core. That's more than twice orrery's whole idle use, 1.1% after the process-stats fix. With
D-Bus, systemd sends a signal when a service changes, and orrery does nothing in between.

**Without sudo, `systemctl` is D-Bus anyway.** `systemctl` asks systemd over D-Bus, and systemd
checks the same polkit rules. So it has every permission limit that D-Bus has, plus the polling.
Only `sudo systemctl` gets around polkit, and that needs sudo rights. Moonraker asks the user
for the sudo password through `machine.sudo.password` when it needs them.

**`zbus` is pure Rust,** and it's the usual D-Bus crate. It adds 1.56 MB to orrery's aarch64
binary: 8.78 MB grew to 10.34 MB in a build that makes Moonraker's calls through zbus's typed
proxies. That's 18%. For comparison, the whole Spoolman agent is 5.3 MB. Most of the extra code
is generated for each type, not zbus's own logic:

| Part | Size |
|---|---|
| Async state machines, one for each call and type | 344 KiB |
| Drop glue for those state machines and messages | 302 KiB |
| Encoding and decoding, for each call's types | 172 KiB |
| zbus itself: connections, login, match rules, streams, its runtime layer, errors | about 600 KiB |

Without the proxies, with `Connection::call_method` and untyped message bodies, zbus still
adds 1.27 MB. So the typed proxies cost about 0.3 MB, and the rest comes with zbus.

**orrery's own client** needs only a small part of D-Bus:

- Connecting to `/run/dbus/system_bus_socket` and logging in with `EXTERNAL`, which sends the
  user id. It's one line of text.
- Method calls and their replies, and the signals orrery subscribes to with `AddMatch`.
- The types those calls use: strings, object paths, integers, booleans, arrays, structs, and
  variants.

The calls are `ListUnitsFiltered`, `StartUnit`, `StopUnit`, `RestartUnit`, `GetUnitByPID`,
`Properties.Get`, the `PropertiesChanged` signal, polkit's `CheckAuthorization`, and logind's
`Reboot`, `PowerOff`, and `Halt`. The D-Bus wire format hasn't changed in years. Tests can run
against a private `dbus-daemon`; the dev machine has one at `/usr/bin/dbus-daemon`, and the
flake could add `pkgs.dbus`.

## What bluev2 has

- **systemd 252**, the Debian 12 version, and a system bus at `/run/dbus/system_bus_socket`.
- **Moonraker used `systemd_dbus`**, its default provider.
- **The polkit rule from Moonraker's installer**, in
  `/usr/share/polkit-1/rules.d/moonraker.rules`. It allows managing units, reboot, shutdown,
  halt, and PackageKit to processes that run as `printer` **and** have the `moonraker-admin`
  group (gid 1001). The `printer` user isn't in that group. `moonraker.service` gets it with
  `SupplementaryGroups=moonraker-admin`.
- **`printer` has passwordless sudo for everything.** That's this printer's setup, not
  something orrery can expect. Most installs ask for a password.
- **Allowed services** (`moonraker.asvc`): `klipper_mcu`, `webcamd`, `MoonCord`,
  `KlipperScreen`, `moonraker-telegram-bot`, `sonar`, `crowsnest`, `octoeverywhere`,
  `mobileraker`, and `aldis`. The ones installed are `klipper`, `klipper-mcu`, `crowsnest`,
  `aldis`, and `moonraker`.

**So orrery has to run as a service to control services.** In the tmux pane, orrery lacks
`moonraker-admin`, and polkit refuses it. That's true for every option except `sudo systemctl`.
orrery gets the group by running from a unit with `SupplementaryGroups=moonraker-admin`, as
Moonraker does. To test without writing a unit file, a transient unit works:

```
sudo systemd-run --unit=orrery-test --uid=printer --gid=printer \
  -p SupplementaryGroups=moonraker-admin -p WorkingDirectory=/home/printer/orrery-test/bin \
  /home/printer/orrery-test/bin/orrery --orrery-config /home/printer/orrery-test/orrery.toml
```

The log is then in `journalctl -u orrery-test`, as well as in `orrery.log`.

## Depending on systemd

Only service control depends on systemd: the service list and its states, start, stop, and
restart, reboot and shutdown through logind, and agents started with `systemd_unit`. Files, the
API, Klipper, the watcher, and `command` agents don't use it. orrery supervises `command`
agents itself, so agents work on any init system.

| Platform | Init system | Service control |
|---|---|---|
| Raspberry Pi OS, Debian, Armbian, Ubuntu, NixOS | systemd | the `systemd_dbus` provider |
| Docker images | often supervisord | Moonraker has `supervisord_cli` |
| Creality K1 and Sonic Pad (to be checked) | BusyBox init, OpenWrt's procd | `none` for now |
| Alpine, Void | OpenRC, runit | `none` for now |

**The rule:** service control is one interface with a provider per system, and nothing outside
it may use systemd. systemd is the only provider at first, beside `none`. A provider for another
system can come when someone needs it. If providers multiply, each one could become a separate
component, as Spoolman became an agent. That isn't needed while there's one.

## Recommendation

1. **Use D-Bus with polkit,** as Moonraker's default provider does. It's the only option with
   no polling, and it works with the polkit rule that Moonraker's installer already put in
   place, so a Moonraker install needs no new permissions.
2. **Write orrery's own small client** instead of using `zbus`. The calls are few and fixed,
   and even zbus without its proxies adds 1.27 MB, about a seventh of orrery. orrery's client
   encodes messages with one function that isn't generic, so new calls add almost no code. If
   the client turns out harder than expected, `zbus` can replace it behind the same functions.
3. **Keep the providers Moonraker has:** `systemd_dbus` by default, and `none` to turn service
   control off. `systemd_cli` and `supervisord_cli` can come later, if someone needs them;
   until then, choosing them gives a warning that they aren't supported.
4. **When D-Bus isn't there or polkit refuses,** add a warning for each refused action, as
   Moonraker does, and keep everything else working.

## Deployment

**Decision (2026-11-20):** orrery gets its own `orrery.service`, a system unit that runs as the
printer's user. A user unit can't work: polkit trusts orrery by a supplementary group, and only a
system unit can grant one. A rule that trusted the user alone would also trust every shell that
user opens.

**The service is named `orrery`,** not `moonraker`. `instance_ids` still has the `moonraker`
key, with orrery's unit as its value: `{moonraker: "orrery", klipper: "klipper"}`. In the API,
that key means "the API server's unit", so this isn't orrery passing itself off as Moonraker;
the field name is the API's. Both values come from systemd, as in Moonraker: orrery asks which
unit owns its own process, and which unit owns Klippy's process, whose id it gets from the
unix socket when Klippy connects. On a machine with several printers, such as `klipper-2`
beside `klipper-1`, that's what points the restart buttons at the right units.

Fluidd uses the key to treat the API server's entry in the service list differently: restarting
it calls `server.restart`, and its stop button is hidden (`src/mixins/services.ts`). The
allowed-services check allows `orrery` services, as it allows `moonraker` and `klipper` ones.
orrery refuses to stop its own unit, as Moonraker does.

**Testing started with Moonraker's group.** The transient `orrery-test` unit takes
`SupplementaryGroups=moonraker-admin`, whose polkit rule Moonraker's installer put in place. It
now takes `orrery-admin` too, when that group exists.

**`sudo orrery setup`** (built 2026-11-22). One command with root, run again whenever what's
needed changes, with `--check` and `--remove`, as `aldis setup` has. A distribution package can
call the same command, so the files have one source. It writes:

- **The group `orrery-admin`,** a system group. The printer's user isn't added to it. Only
  `orrery.service` gets it, so the user's own shells aren't trusted.
- **`/etc/systemd/system/orrery.service`,** which runs orrery as the user who ran sudo, with
  `SupplementaryGroups=orrery-admin`, `Restart=always`, and `Conflicts=moonraker.service`, since
  both listen on port 7125. `ExecStart` runs the binary setup ran from, with the `-d`, `-c`,
  and `--orrery-config` options it was given. Without them, setup keeps the installed unit's
  options, so an upgrade doesn't lose them.
- **`/etc/polkit-1/rules.d/orrery.rules`,** in the local admin's folder. Like Moonraker's rule,
  it checks the user and the group's id in `/proc/<pid>/status`. It covers managing units and
  logind's power-off, reboot, and halt actions. Moonraker's PackageKit actions were left out,
  since orrery didn't update packages; the update manager adds them (see
  `2026-11-22-update-manager.md`).

By default setup only writes the files and reloads systemd, which is also how an upgrade is
applied (user, 2026-11-22). Switching over takes explicit flags:

| Flag | What it does |
|---|---|
| `--disable-moonraker` | Stops and disables `moonraker.service` |
| `--start` | Enables `orrery.service` and starts it, or restarts it if it runs |
| `--migrate` | Both |
| `--strict` | Writes the strict rule (below) |
| `--check` | Reports what's in place and changes nothing; works without sudo |
| `--remove` | Stops and disables `orrery.service`, then removes the unit, the rule, and the group |

**polkit 0.105** (Debian 11, Raspberry Pi OS Bullseye) has no JavaScript rules, so a rule can't
check a process's groups. Moonraker's installer then trusts the user in every process. orrery's
setup refuses instead, and says that Moonraker's rule and group keep working (user, 2026-11-22).

**Checked on bluev2 (2026-11-22),** with polkit 122:

- setup, `--check`, a second run ("already current"), `--migrate`, and `--remove`;
- polkit loaded both rules without errors;
- `pkcheck` allows orrery's process to reboot, power off, and manage units, and refuses an SSH
  shell of the same user;
- `instance_ids.moonraker` is `orrery`, and restarting crowsnest through the API works;
- with `--strict`, polkit allows `crowsnest`, `klipper`, and `klipper-mcu`, and refuses `ssh`
  and `dbus`.

- **Plugins declare, setup applies** (not built yet). Most plugins only need a service restarted, and they ask
  orrery for that over the socket. orrery checks its allowed-services list and makes the D-Bus
  call itself, so only orrery needs bus permissions. A plugin that needs more, such as aldis
  with its udev rules and narrow sudoers file, lists it in its manifest, and `orrery setup`
  writes it. Plugins never run setup code as root. A `server.info` warning names what an
  installed plugin needs that isn't in place yet.
- **One renderer per system,** as with the providers. systemd and polkit come first.
- **A normal rule and a strict one,** both built. The normal rule allows managing every unit,
  as Moonraker's does. The strict rule allows only the units on orrery's allowed-services list:
  systemd tells polkit which unit each request is for (`action.lookup("unit")`). The list is
  `moonraker.asvc`, names that start with `klipper`, `moonraker`, or `orrery`, and agents'
  system units, read as orrery reads them. With the strict rule, a new service or plugin
  means running setup again. A rule that reads the list while it runs would avoid that,
  but it gains little security, because the list it reads would be one orrery can write.

## The allowed-services list

orrery reads its allowed services from `moonraker.asvc` in the data folder, as Moonraker does,
and writes Moonraker's default list there when the file is missing. That's a Moonraker
compatibility feature for now (user, 2026-11-22). Moonraker's installers and plugins add their
services to that file, so reading it keeps them working.

**Later:** orrery moves the list to its own config, such as an `allowed` list in `orrery.toml`.
`moonraker.asvc` then becomes one of the list's sources, read only while Moonraker
compatibility is on, as `[moonraker_compat]` does for the legacy API key. The strict polkit rule
reads the same list, so it follows.

## Open questions

1. **A sudo fallback.** Moonraker's D-Bus provider falls back to `sudo` for reboot and
   shutdown when logind isn't available. orrery leaves it out for now: without logind, the
   calls fail with an error that says so.
2. **Restarting orrery itself. Decided (2026-11-20):** orrery shuts down as for SIGTERM, which
   stops its `command` agents, then starts its own program again in the same process. That
   works in a service, in tmux, and from a terminal, and needs no polkit permission.
   Moonraker restarts its unit through systemd instead.
