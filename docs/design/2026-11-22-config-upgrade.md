# Config upgrades

**Status (2026-11-22):** reviewed by the user, who answered the open questions. One question
is left: where the table of option types lives. Nothing is built.

As orrery matures, its options will move: a section gets renamed, one option splits in two, or
something moves into `[moonraker_compat]`. Making every user edit `orrery.toml` by hand for each
change would be a pain (user, 2026-11-22). So orrery gets one tool that upgrades a config to
the current version. Converting a `moonraker.conf` is that tool's first step, not a separate
converter.

## Summary

- **One command, `orrery config upgrade`,** brings `orrery.toml` to the current version, one
  step at a time. The first step copies settings from `moonraker.conf`.
- **`moonraker.conf` stays** (user, 2026-11-22). The conversion only copies settings out of it.
  orrery keeps reading it, and `orrery.toml` wins where both set an option, as now. Deleting it,
  or telling the user they can, is for much later.
- **`orrery.toml` gets a version,** and each step is a small, tested function.
- **orrery itself runs the same steps in memory** when it starts with an older file, and warns.
  So an upgraded orrery still starts, and its components only ever read the current version.
- **Comments and layout survive,** through `toml_edit`. That's the library the later "orrery
  edits `orrery.toml`" item needs too (see the logins note).

## The command

```
orrery config upgrade [--dry-run]
```

It takes the same `-d`, `-c`, and `--orrery-config` options as the server, to find the files.
It:

1. reads `orrery.toml` and its version, and `moonraker.conf` when there is one;
2. runs each step from that version to the current one;
3. prints what each step changed, one change a line, such as "Copied [server] host from
   moonraker.conf." or "Renamed [authorization] enable_api_key to [moonraker_compat]
   legacy_api_key.";
4. writes `orrery.toml`, unless `--dry-run` is given.

A file that's already current is left alone, and the command says so. Running it twice changes
nothing the second time.

**Writing safely:**

- It refuses to write if `orrery.toml` changed while it ran, for example in Fluidd's editor.
- It writes through a temporary file and a rename.
- It keeps the old file as `orrery.toml.bak` next to it. The config folder has no secrets, so
  printerkeep and Klipper-Backup can commit both, and their history shows each upgrade.
- It runs as the printer's user, so the file keeps its owner. `sudo orrery setup` doesn't
  rewrite the config as root; it says when the config needs upgrading and gives the command.

## Versions

`orrery.toml` names its version:

```toml
[orrery]
config_version = 1
```

- **A file without it is version 0.** So is a printer with only a `moonraker.conf`.
- **Each step takes version N to N+1.** It works on the `toml_edit` document, and can read
  `moonraker.conf` through `config.rs`'s parser. It reports each change it makes.
- **Old steps are dropped eventually,** and when is decided case by case (user, 2026-11-22).
  Until then, a printer that skipped several releases still upgrades in one run. After a drop,
  there's a gap: orrery can't upgrade a file older than its oldest step. It then says so, and
  names the last orrery release that upgrades that version:

  > orrery.toml is version 1, and this orrery upgrades only from version 3. Upgrade it with
  > orrery 0.9 first, then run `orrery config upgrade` again.

**At startup,** when `orrery.toml` is older than the current version, orrery runs the same steps
on a copy in memory, then reads the result as usual. It writes nothing. It warns once, in the
log and in `server.info`'s warnings:

> orrery.toml is version 1, and this orrery reads version 2. It works, but run `orrery config
> upgrade` to save the new version.

That keeps one code path. The components never parse old layouts, and the steps are tested
every time orrery starts with an old file. A step that fails at startup stops orrery, with the
step's message and the command to run, since the config would otherwise mean something
different than the user wrote.

## Step 0 to 1: settings from moonraker.conf

orrery already reads `moonraker.conf` with the same section and option names as `orrery.toml`
(`crates/orrery/src/config.rs:1-11`). So most of this step is a copy, with two parts that need
care.

**Types.** `moonraker.conf` values are text. `orrery.toml` values are typed, so the step has to
know each option's type: `port: 7125` becomes a number, `force_logins: True` a boolean, and an
indented list such as `trusted_clients` an array. orrery knows the types only where its
components read the options. The plan is a table of the options orrery reads, with their types.
Today they're spread over getter calls with defaults, and `MOONRAKER_OPTIONS` lists Moonraker's
options that orrery doesn't read (`config.rs:465`). The same table could later check `orrery.toml`
for unknown options, which orrery doesn't do now.

**What's copied:**

- **Options orrery reads,** with their comments from `moonraker.conf` carried over as TOML
  comments.
- **Renamed options:**
  - `[authorization] enable_api_key` goes to `[moonraker_compat] legacy_api_key`: `True`
    becomes `"admin"`, and `False` becomes `false`.
  - Only with `--convert-trusted-clients` (user, 2026-11-22): `[authorization]
    trusted_clients` becomes `[authorization.networks] admin = [...]`, which says the role
    outright, as the authorization note prefers. Host names, which orrery skips today, are left
    out with a line in the report. Without the flag, `trusted_clients` is copied as it is, and
    keeps Moonraker's meaning: every listed network is admin.
- **Sections orrery handles differently,** such as `[spoolman]`, which becomes the built-in
  agent table. Each one is listed in this note before it's built.

**What isn't copied:** sections and options orrery doesn't use, such as `[power]`, or
Moonraker's options in `MOONRAKER_OPTIONS`. The report lists them, so the user knows they have
no effect. They stay in `moonraker.conf`.

**`[include]` files** are read, as orrery reads them now. Their settings go into the one
`orrery.toml`, and each line of the report names the file a setting came from.

## When both files set an option

After the conversion, `moonraker.conf` and `orrery.toml` both have each copied option, and
`orrery.toml` wins. But Moonraker's installers and plugins still edit `moonraker.conf`. For
example, crowsnest's installer adds to `[update_manager]`, and a user may change `[server] port`
there out of habit. A change orrery ignores without a word would be confusing.

**So orrery warns at startup** about each option whose value differs between the two files:

> moonraker.conf sets [server] port to 7126, but orrery.toml's 7125 wins. Change it in
> orrery.toml, or remove it there to use moonraker.conf's.

The warning appears once per option, in the log and in `server.info`. Options with the same
value in both files say nothing.

## Edits don't upgrade

When orrery later edits `orrery.toml` itself, such as to give a user a role, it changes only
what was asked for. It never upgrades the file on the side: the config is the user's, and
rewriting it while doing something else doesn't respect that (user, 2026-11-22). An edit to a
part whose layout differs between the file's version and the current one is refused, with the
command to upgrade first.

## Footprint

`toml_edit` comes from the same project as the `toml` crate orrery uses, and `toml` already
pulls in its parser. Its size on the aarch64 musl build gets measured before it's chosen, as
for the login crates. If it's too big, the tool could live in `orrery-import` or a tool of its
own, and orrery's startup would run the steps on the `toml` crate's values instead, losing only
the comments, which it never writes anyway.

## Later: a config tool for installers

Installers and plugins edit the config themselves today. For example, crowsnest's installer
appends its `[update_manager crowsnest]` section to `moonraker.conf`. A script that edits TOML
with `echo` or `sed` can easily break the file, and then orrery doesn't start. So orrery gets
a command that installers call instead (user, 2026-11-22). It edits `orrery.toml` through
`toml_edit`, as `orrery config upgrade` does, so comments and layout survive.

What it needs, roughly:

- **Set and remove** one section or option, such as an update manager entry.
- **Check before writing:** the result must parse and pass orrery's own checks, or nothing is
  written and the command says why.
- **Run it again safely:** adding an entry that's already there with the same values changes
  nothing, so an installer that runs twice does no harm.
- **Write in one step,** to a temporary file renamed over the old one, so a crash leaves the
  old file.

The command's name and options get designed when it's built. Until then, installers keep
writing to `moonraker.conf`, and orrery reads it as a fallback.

## Testing

- **Each step** gets unit tests with a before file and an after file, including comments.
- **A round trip:** upgrading the current version's example changes nothing.
- **Startup:** orrery with a version 0 `orrery.toml` reads the same settings as with the upgraded
  file. A conformance case checks `server.config`'s values for both.
- **On bluev2:** a `--dry-run` on a copy of its `moonraker.conf` and `orrery.toml`, which the
  user reviews before anything is written.

## Open questions

- Where does the table of option types live: next to each component's getters, or in one place
  in `config.rs`?
