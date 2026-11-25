# Config reload and export

**Proposal (2026-11-25), for later.** Nothing is decided or built yet. Both build on the
conversion in [Config upgrades](2026-11-22-config-upgrade.md): Orrery reads `moonraker.conf`
only through a conversion to `orrery.toml` settings, run in memory, with `orrery.toml` laid over
it one option at a time. The result is one set of typed settings, which is what both ideas need.

## Reload while running

Moonraker reads its config at startup, and a change needs a restart. Orrery's one read path
makes a reload straightforward: read both files again, convert and merge, compare with the
settings in use, and apply the difference.

What a reload could apply, and what still needs a restart:

| Area | On reload |
|---|---|
| Permissions: networks, keys, users' roles, trusted proxies | Applied. Connections get their new access, as after a key rotation |
| Actions and buttons | Applied. `orrery-gpio` gets its new buttons |
| Agents and their settings | Applied. A changed agent gets a "config changed" notice, or is restarted; a new one starts, a removed one stops |
| Update manager entries | Applied, through the update agent's settings |
| Port, unix socket, data folder, log file | A restart |

Open questions:

- **What starts it:** the file watcher seeing either file change, a command such as
  `orrery config reload`, or both. Fluidd and Mainsail edit `moonraker.conf` and then offer to
  restart Moonraker; whether that restart can become a reload.
- **A config with errors:** keep the settings in use, and report the errors as warnings.
- **How agents hear of it:** a notification and a fresh `orrery.agent.config`, or a restart of
  the agent. An agent that doesn't know the notice is restarted.

## Export in one canonical form

The merged settings, written out as `orrery.toml`: every option Orrery uses, from either file,
typed, in a fixed order, with nothing it ignores. The same settings always give the same text.

Uses:

- **The startup log,** as `klippy.log` starts with Klipper's config: the settings Orrery runs
  with, in one normalised form, so a debug log shows them without the two files.
- **Inspection,** with a command such as `orrery config show`, to see what Orrery makes of the
  two files together.
- **Conversion:** the conversion writes the same form, so there's one way to show settings.
- **Statistics:** an anonymised export, for seeing which options people use. It keeps section
  and option names, and replaces values with their kind, or with nothing. Sharing it would be
  opt-in.

Secrets stay out of every export, with keys and passwords hidden. The anonymised one also leaves
out host names, addresses, URLs, paths and names, such as users', buttons' and entries' names.
