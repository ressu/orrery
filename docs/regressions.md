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
  access**, including gcode and file changes. Run orrery only on a network you trust.
  Every client gets what Moonraker gives a trusted client: `access.get_user` returns
  `_TRUSTED_USER_`, and `access.info` says `trusted: true`. `access.oneshot_token` hands out
  tokens so frontends can build download links, and orrery doesn't check them. Logins, user
  accounts, JWTs, and the API key aren't there.
  CORS follows `cors_domains` as in Moonraker, including the fallback for an origin whose host
  is a trusted IP. Host names in `trusted_clients` don't count for that fallback.
- **Why:** authorization comes later; see `conformance/coverage.md`.
- **Cases:** `server/config` (it expects an `authorization` section), and the authorization
  cases when they're written. `access/trusted` covers the trusted-client replies.

### partial-server-config: `server.config` leaves out components orrery doesn't have

- **Area:** config
- **Moonraker:** `server.config` reports every option of every loaded component, with defaults.
- **orrery:** it reports the options of Moonraker's default components, with Moonraker's
  defaults, in Moonraker's order, including options orrery ignores, such as `ssl_port`. Sections
  of optional components that orrery doesn't have, such as `[update_manager]` or `[power]`,
  aren't listed. `file_system_observer` is `none`, because orrery has no file watcher yet.
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

### no-file-observer: changes made outside the API aren't seen right away

- **Area:** files
- **Moonraker:** watches the roots with inotify. When a file changes on disk, it sends
  `notify_filelist_changed` and reads the new file's metadata.
- **orrery:** works like Moonraker with `file_system_observer: none`. It sees the changes made
  through its API, and it reads metadata for the gcodes folder when it starts. A file copied
  in with scp, or a `printer.cfg` that Klipper saves, shows up when a client lists the folder
  again. Its metadata is read the first time a client asks for it with
  `server.files.metadata`, or with `server.files.metascan`. The same request also gets
  metadata for a file that the startup scan hasn't reached yet. Moonraker answers 404 until
  its scan gets there.
- **Why:** the API cases come first. A watcher can follow.
- **Cases:** none yet.

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

### log-file: orrery's log is `orrery.log`, with no log rollover endpoint

- **Area:** files, server
- **Moonraker:** writes `logs/moonraker.log` and serves it at `/server/files/moonraker.log`.
  It starts a new file at local midnight and keeps two old ones. `server.logs.rollover` starts
  a new Moonraker or Klipper log on request.
- **orrery:** writes `logs/orrery.log`, so it doesn't mix with Moonraker's log, and serves it
  at `/server/files/moonraker.log`. It starts a new file at midnight UTC and keeps two old
  ones. `-l` sets another file and `-n` turns the file off, as in Moonraker. There's no
  `server.logs.rollover`, so Fluidd's rollover button fails.
- **Why:** frontends download the log by its Moonraker address. Rollover can follow.
- **Cases:** `files/log_downloads`.

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

### no-service-control: no service list, reboot, or shutdown

- **Area:** API
- **Moonraker:** with a systemd or supervisord `provider`, `machine.system_info` lists the
  services clients can control, with their state. `machine.services.*`, `machine.reboot`, and
  `machine.shutdown` control them.
- **orrery:** lists no services, as Moonraker does with `provider: none`, and has none of the
  control endpoints yet. `instance_ids` are always `moonraker` and `klipper`. For
  virtualization, orrery runs `systemd-detect-virt` instead of asking systemd over D-Bus.
- **Why:** service control comes later.
- **Cases:** none.

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
