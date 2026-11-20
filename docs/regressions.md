# Known differences from Moonraker

This file lists every known way orrery behaves differently from Moonraker, so the differences are
deliberate and tracked. Each entry has an id. A conformance case that fails because of an entry is
marked `known_regression: <id>`. To fix a difference, remove its entry and the marks together.

Most missing features are simply not written yet. Those show up as failing cases in the
conformance results, not here. This file lists the differences that are deliberate, or that
change how safe or complete orrery is to run.

## Entry format

```markdown
### <id>: <short title>

- **Area:** config, datastore import, API, or build
- **Moonraker:** what Moonraker does
- **orrery:** what orrery does instead
- **Why:** the reason, such as too much effort for now or out of scope
- **Cases:** conformance case ids marked with this regression
```

## Entries

### no-auth: no authorization

- **Area:** API
- **Moonraker:** `[authorization]` controls access: trusted networks, user logins, JWTs, and an
  API key.
- **orrery:** there's no authorization yet. **Every client that can reach the port has full
  access**, including gcode, file changes, starting and stopping services, restarting orrery,
  and rebooting or shutting down the machine. Run orrery only on a network you trust.
  Every client gets what Moonraker gives a trusted client: `access.get_user` returns
  `_TRUSTED_USER_`, and `access.info` says `trusted: true`. `access.oneshot_token` hands out
  tokens so frontends can build download links, and orrery doesn't check them. Logins, user
  accounts, JWTs, and the API key aren't there.
  CORS follows `cors_domains` as in Moonraker, including the fallback for an origin whose host
  is a trusted IP. Host names in `trusted_clients` don't count for that fallback.
- **Why:** authorization is being designed from scratch, not copied from Moonraker; see
  `docs/design/2026-11-20-authorization.md`.
- **Cases:** `server/config` (it expects an `authorization` section), and the authorization
  cases when they're written. `access/trusted` covers the trusted-client replies.

### partial-server-config: `server.config` leaves out components orrery doesn't have

- **Area:** config
- **Moonraker:** `server.config` reports every option of every loaded component, with defaults.
- **orrery:** it reports the options of Moonraker's default components, with Moonraker's
  defaults, in Moonraker's order, including options orrery ignores, such as `ssl_port`. Sections
  of optional components that orrery doesn't have, such as `[update_manager]` or `[power]`,
  aren't listed.
- **Why:** clients read values from this list: Fluidd reads `spoolman.server` to connect to
  Spoolman itself, and `data_store.temperature_store_size` for its charts. Matching Moonraker's
  whole list keeps clients that weren't checked working too. A listed option isn't a promise
  that orrery uses it; this file lists what orrery ignores.
- **Cases:** `server/config` passes.

### datastore: orrery keeps its own store

- **Area:** datastore import
- **Moonraker:** keeps its data in SQLite, `database/moonraker-sql.db`.
- **orrery:** keeps its data in redb, `database/orrery.redb`, and doesn't read Moonraker's file.
  Database backups are copies of the redb file. Importing Moonraker's data will be a separate
  tool.
- **Why:** see `docs/design/2026-11-01-datastore-spike.md`.
- **Cases:** none; the API behaves the same.

### file-watcher: the file watcher announces outcomes, not every step

- **Area:** files
- **Moonraker:** watches the roots with inotify, the kernel's way of reporting changes to
  folders. When a file changes on disk, it sends `notify_filelist_changed` and reads a new gcode
  file's metadata. It holds back the notification for each API change for a second, so the
  watcher's can replace it.
- **orrery:** watches the roots too, and sends the same notifications, with these differences:
  - It waits until the folders have been quiet for 0.25 s, and announces the outcome. A file
    created and removed in that time, such as a lock file, isn't announced. A file written and
    then renamed into place is one `create_file`, at its new name. Moonraker announces each
    step: `create_file` for the temporary name, then `move_file`.
  - It doesn't ask the kernel for each write, only for "closed after writing", so it doesn't
    wake for every line Klipper adds to its log. A file is announced once it's closed, as in
    Moonraker. A file still open, such as a log, shows up when a client lists the folder again.
  - API changes are announced at once, not a second later.
  - The thumbnails orrery writes into `.thumbs` aren't announced. Moonraker announces them, and
    Fluidd hides that folder.
  - If the kernel's queue of changes overflows, orrery watches every folder again and sends
    `root_update` for each root. Moonraker misses those changes.
  - `file_system_observer: none` turns the watcher off, as in Moonraker.
- **Why:** clients need to know what's in the folders, not each step that got there. Fewer
  wakeups matter on a printer's small computer.
- **Cases:** `files/watch` (both servers), `files/watch_orrery` (orrery only).

### partial-metadata: some slicers get only basic metadata

- **Area:** files
- **Moonraker:** reads metadata from PrusaSlicer and its forks, Slic3r, Cura, BambuStudio,
  Simplify3D, KISSlicer, ideaMaker, IceSL, and Kiri:Moto. It also unpacks `.ufp` uploads, runs
  gcode file processors, and can add object definitions (`enable_object_processing`).
- **orrery:** reads PrusaSlicer and its forks (SuperSlicer, OrcaSlicer, and others), Slic3r,
  Slic3r PE, BambuStudio, and Cura. A file from another slicer gets only the size, times, first
  layer height, and object height. orrery refuses `.ufp` uploads, and it has no file processors
  or object processing.
- **Why:** these are the slicers most printers use. The others can be ported one at a time.
- **Cases:** `files/metadata`, `files/metadata_plain`, `files/metascan`.

### missing-file-roots: no `config_examples` or `docs` roots

- **Area:** files
- **Moonraker:** adds the `config_examples` and `docs` roots from Klipper's folder.
- **orrery:** has the `config`, `logs`, and `gcodes` roots.
- **Why:** frontends work without them.
- **Cases:** none yet.

### log-file: orrery's log is `orrery.log`

- **Area:** files, server
- **Moonraker:** writes `logs/moonraker.log` and serves it at `/server/files/moonraker.log`.
  It starts a new file at local midnight and keeps two old ones. `server.logs.rollover` starts
  a new Moonraker or Klipper log on request.
- **orrery:** writes `logs/orrery.log`, so it doesn't mix with Moonraker's log, and serves it
  at `/server/files/moonraker.log`. It starts a new file at midnight UTC and keeps two old
  ones. `-l` sets another file and `-n` turns the file off, as in Moonraker.
  `server.logs.rollover` starts a new orrery log, and names the old one for the time, as
  Moonraker names a Klipper log it rolls over. Moonraker instead names it for the day, and
  replaces an earlier rollover from the same day. Klipper's log is rolled over as in
  Moonraker, with the differences in `no-service-control`.
- **Why:** frontends download the log by its Moonraker address.
- **Cases:** `files/log_downloads`, `server/logs_rollover`.

### history-aux-fields: history's auxiliary fields come from agents, without totals

- **Area:** history
- **Moonraker:** the `[sensor]` and `[spoolman]` components add auxiliary fields to each job,
  such as energy used or the spool, and their totals. Each field has a tracking strategy, such
  as collecting values or adding them up.
- **orrery:** has neither component. Agents set fields instead, with `orrery.agent.history_field`
  while a job is recorded, and only the fields their `[agent.<name>]` table lists in
  `history_fields`. An agent sends the field's whole value; orrery doesn't track it.
  `auxiliary_totals` is always empty.
- **Why:** an agent knows its own values, so orrery doesn't need Moonraker's trackers.
- **Cases:** `agents/history_field` (orrery only).

### spoolman-agent: Spoolman is a separate program, set up in `orrery.toml`

- **Area:** Spoolman
- **Moonraker:** a `[spoolman]` section turns on the built-in component.
- **orrery:** the `orrery-spoolman` agent does the same work, from the same section. orrery
  starts it from its own folder when `[spoolman]` has a `server`; without the program, Spoolman
  stays offline and `server.info` has a warning. Other differences:
  - The agent keeps the active spool in the `spoolman` database namespace, not in `moonraker`.
    On its first run, it takes the spool Moonraker had.
  - A job gets its `spool_ids` field only while the agent is connected. Moonraker adds the field
    to every job.
  - When Spoolman sends no reply at all, a `proxy` request with `use_v2_response` gets
    `status_code` 500 and the error's text.
- **Why:** plugins are separate programs (docs/design/2026-11-15-plugin-interface.md).
- **Cases:** `agents/builtin_spoolman` (orrery only); none against Moonraker yet. `crates/orrery-spoolman/tests/agent.rs` checks the
  agent against stand-ins for orrery and Spoolman.

### no-python-info: `machine.system_info` has no `python` field

- **Area:** API
- **Moonraker:** `system_info.python` gives the version of the Python that runs Moonraker.
- **orrery:** leaves the field out. orrery isn't a Python program, and a made-up version would
  mislead anyone who reads it.
- **Why:** there's no true value to give.
- **Cases:** `machine/system_info`.

### no-service-control: no systemd_cli or supervisord_cli provider

- **Area:** API
- **Moonraker:** with a systemd or supervisord `provider`, `machine.system_info` lists the
  services clients can control, with their state. `machine.services.*`, `machine.reboot`, and
  `machine.shutdown` control them.
- **orrery:** with `provider: systemd_dbus`, the default, it lists the allowed services and
  follows their state over D-Bus, with `notify_service_state_changed`, and has
  `machine.services.start`, `stop`, and `restart`, and `machine.reboot` and `machine.shutdown`
  through logind. `instance_ids` come from systemd, as in Moonraker. It doesn't have the
  `systemd_cli` and `supervisord_cli` providers yet. Small differences:
  - Klipper's log rollover waits for Klipper to stop before it renames the log, refuses while
    a print is paused as well as while it prints, and starts Klipper again when the rename
    fails. Moonraker renames as soon as the stop is queued, and leaves Klipper stopped when the
    rename fails. The time in the old log's name is UTC.
  - Without logind, or with `provider: none`, reboot and shutdown fail. Moonraker falls back
    to `sudo systemctl reboot`, with the sudo password it asks clients for.
  - orrery gives one warning that lists every polkit action it isn't allowed, instead of one
    warning per action.
  - Restarting orrery's own service, and `server.restart`, start orrery's program again in the
    same process, after the usual shutdown. Moonraker restarts its service through systemd,
    and restarts itself inside Python for `server.restart`. With `provider: none`, Moonraker
    answers `ok` to a restart of its own service and does nothing; orrery restarts.
  - When polkit refuses an action, the error is 403 and says so. Moonraker passes on the D-Bus
    error.
  - orrery lists only `.service` units. Moonraker takes every unit type and cuts the name at its
    first dot, so a `klipper.socket` could stand in for `klipper.service`.
  - orrery always allows services whose names start with `orrery`, beside `moonraker` and
    `klipper` ones.
  - Agents' units are listed too, so clients can restart an agent alone. An agent's user unit
    is listed as `user/<unit>`, such as `user/orrery-spoolman`, which no Moonraker service
    name can be.
  - For orrery or Klippy started outside a service, such as from a terminal, its
    `instance_ids` entry is `(unknown)`, which can't match a real service. Moonraker gives the
    scope's name, such as `session-3`, and falls back to `moonraker` and `klipper`. A fallback
    could name another printer's Klipper on a machine with several, and an empty id makes
    Fluidd fall back the same way.
  - Moonraker saves Klipper's unit in its database, so `instance_ids.klipper` is right before
    Klippy connects. orrery finds it again on each connection.
  - For virtualization, orrery runs `systemd-detect-virt` instead of asking systemd over D-Bus.
- **Why:** service control comes in steps; see `docs/design/2026-11-19-service-control.md`.
- **Cases:** `machine/services` covers the refusals. The conformance suite runs with
  `provider: none`, so the actions were checked by hand; the D-Bus client is tested against a
  private `dbus-daemon`.

### import-gaps: what `orrery-import` doesn't bring over

- **Area:** datastore import
- **Moonraker:** keeps users, its own state, and the gcode metadata cache in `moonraker-sql.db`.
- **orrery:** `orrery-import` skips these:
  - Users and their API key, because orrery has no authorization yet. The import reports how
    many users it skipped.
  - The `moonraker` and `database` namespaces: Moonraker's own state, such as its instance id
    and the update manager's data. orrery keeps its own.
  - `gcode_metadata`: orrery reads the files again instead. It still copies each file's link to
    its last print (`job_id` and `print_start_time`) when the file hasn't changed.
  - The legacy LMDB database (`moonraker.db/`), from Moonraker before v0.9.
- **Why:** users come with authorization. The rest belongs to Moonraker, or orrery rebuilds it.
- **Cases:** none; the import's unit tests read `conformance/fixtures/import/seed.db`.

### agent-null-result: an agent's `null` result is a result

- **Area:** API
- **Moonraker:** when a client calls an agent with `server.extensions.request` and the agent
  replies `"result": null`, Moonraker returns an agent error (424, "Agent <name> RPC error").
  Its check reads a missing `result` and a `null` one the same way.
- **orrery:** returns the `null` as the result. JSON-RPC allows a `null` result, and a reply is
  an error only when it has an `error` member. A reply with neither is an agent error in both.
- **Why:** Moonraker's behaviour looks accidental: its docs don't mention it, and agents such as
  aldis work around it. Agents written for Moonraker never reply `null`, so none of them change.
- **Cases:** `extensions/null_result`.

### webcam-test-no-tls: `server.webcams.test` can't reach `https` snapshots

- **Area:** API
- **Moonraker:** fetches the snapshot URL over HTTP or HTTPS to report `snapshot_reachable`.
- **orrery:** has no TLS, so an `https` snapshot URL is always reported as unreachable. Plain
  `http` URLs work as in Moonraker.
- **Why:** orrery stays free of C dependencies, and the check is only a hint for clients.
- **Cases:** none.

## Planned entries

These differences are already decided in the plan. Each gets an entry when the code that
introduces it lands:

- No TLS: `ssl_certificate_path` and `ssl_key_path` are ignored.
- `moonraker.conf` support is best effort: `[secrets]`, Jinja templating, and full option
  validation come later.
- Legacy LMDB databases from before Moonraker v0.9 can't be imported.
- Out-of-scope components (update manager, power, sensors, MQTT, and others) are missing or
  stubbed.
