# Orrery compared with Moonraker

What you lose, what you gain, and what to look out for when you switch from Moonraker. It's an
overview, not a list of everything Orrery does: the details are in the
[configuration docs](configuration/README.md) and in [regressions.md](regressions.md), which lists each
difference the conformance cases check. What's missing here is also the list to build from.

## What you lose

- **TLS.** Orrery serves plain HTTP; put it behind a proxy such as nginx for HTTPS.
  `ssl_certificate_path` and `ssl_key_path` are ignored, and `server.webcams.test` reports an
  `https` snapshot URL as unreachable.
- **Components.** Power devices, sensors, MQTT, and other components outside Orrery's scope
  are missing. `server.config` leaves their sections out.
- **LDAP logins.** `[ldap]` is ignored, and LDAP users can't log in yet.
- **Slicer metadata.** Full metadata comes from PrusaSlicer and its forks, Slic3r, BambuStudio,
  and Cura. Files from other slicers get only the size, times, and layer and object heights.
  No `.ufp` uploads, gcode file processors, or object processing.
- **File roots.** No `config_examples` or `docs` roots; frontends work without them.
- **Update manager entries.** No `web`, `zip`, or `python` entries, and no `moonraker` entry:
  Orrery updates itself instead. Debian packages need PackageKit; there's no `sudo apt-get`
  fallback.
- **Service control.** Only the `systemd_dbus` provider. Without logind, reboot and shutdown
  fail; Moonraker falls back to `sudo`.
- **`moonraker.conf` support is best effort.** No `[secrets]`, no Jinja templates, and not
  Moonraker's full option checks.
- **Button templates.** A `[button]` runs [actions](configuration/actions.md), not Jinja. From
  `moonraker.conf`, only a template that's a single `call_method` with fixed arguments converts;
  the others are left out, with a warning.
- **History totals.** `auxiliary_totals` is always empty. Agents set auxiliary fields
  themselves, without Moonraker's trackers.
- **`machine.system_info.python`**, since Orrery isn't a Python program.

## What you gain

- **Roles.** Each network, key, and user has a role, `viewer`, `operator`, or `admin`, and each
  request needs one. Moonraker gives every trusted client full control, including reboots.
  Each agent has its own key. A browser request from another site gets no permissions, and the
  unix socket and database are readable only by Orrery's user.
- **Proxy logins.** A trusted proxy can name the user it logged in.
- **Safer logins.** A lockout after failed logins ends after `login_lockout` minutes, not at a
  restart. A failed login doesn't say whether the name exists. Passwords are hashed with
  Argon2id.
- **Safer updates.**
  - `preflight` checks that a new version runs before it takes effect.
  - Executables keep the replaced program in `.previous/`, so a rollback needs no download.
  - Downloads can be checked against `checksums`.
  - A diverged branch isn't reset away during an update.
  - Each service restarts once, and never during a print.
  - Update checks run weekly by default.
- **More update entries.** `archive` entries install every file of a release's archive.
  An entry's `managed_services` adds its service to the service list, including a user unit
  such as `user/aldis`.
- **Buttons with more events.** `on_click` and `on_long_press` as well as press and release,
  each running named actions that Klipper's macros and clients can run too.
- **A quieter file watcher.** It announces the outcome, not every step, and doesn't wake for
  every line Klipper writes to its log. A full kernel queue rescans instead of missing
  changes.
- **Agents.** Spoolman, the update manager, and buttons are separate programs, started by
  Orrery, and any program can join as an agent.

## What to look out for

- **Your data.** Orrery keeps its own database, `database/orrery.redb`, and doesn't read or
  change Moonraker's. `orrery-import` brings over Moonraker's settings, webcams, print history,
  job queue, API key, and users with their passwords
  ([Importing Moonraker's data](install.md#importing-moonrakers-data)). It leaves out:
  - users' signing keys, so everyone logs in again, with the same password;
  - users' groups, which Moonraker doesn't use;
  - Moonraker's own state, the update manager's state, and the gcode metadata cache, which
    Orrery rebuilds;
  - the data of a Moonraker older than v0.9.0, in `moonraker.db/`; updating Moonraker and
    starting it once moves it to the file the import reads.
- **Permissions after the switch.** With `moonraker.conf` alone, trusted networks and the API
  key are `admin`, as before. Once you give roles, Fluidd's Authentication settings need
  `admin`. Host names in `trusted_clients` are skipped, with a warning.
- **A proxy with a role.** A proxy on a network with a role that sends no proxy headers makes
  every caller look like the proxy, as in Moonraker. Orrery warns about it at startup.
- **The log** is `logs/orrery.log`, still served at Moonraker's address. A new file starts at
  midnight UTC.
- **Spoolman's active spool** starts empty, since Moonraker's isn't imported.
- **Agents written for Moonraker** work unchanged. One difference: a `null` result is passed on
  as a result, where Moonraker returns an error.
