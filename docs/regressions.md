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
- **Why:** authorization comes later; see `conformance/coverage.md`.
- **Cases:** `server/config` (it expects an `authorization` section), and the authorization
  cases when they're written.

### no-unix-socket: no unix socket API

- **Area:** API
- **Moonraker:** also serves its API on `comms/moonraker.sock`, for local agents and tools.
- **orrery:** serves only HTTP and the websocket.
- **Why:** no frontend uses the unix socket.
- **Cases:** none yet.

### partial-server-config: `server.config` lists only orrery's options

- **Area:** config
- **Moonraker:** `server.config` reports every option of every loaded component, with defaults.
- **orrery:** it reports the options orrery reads, so sections for components orrery doesn't
  have are missing.
- **Why:** reporting options orrery ignores would claim support it doesn't have.
- **Cases:** `server/config` (snapshot differences).

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

### upload-queue: uploads can't add a print to the job queue

- **Area:** files
- **Moonraker:** with `queue_gcode_uploads: True`, an upload with `print=true` goes into the job
  queue when a print can't start right away.
- **orrery:** has no job queue yet, so `print_queued` is always false.
- **Why:** the job queue comes later.
- **Cases:** `files/upload_print` (the print-now part).

## Planned entries

These differences are already decided in the plan. Each gets an entry when the code that
introduces it lands:

- No TLS: `ssl_certificate_path` and `ssl_key_path` are ignored.
- `moonraker.conf` support is best effort: `[secrets]`, Jinja templating, and full option
  validation come later.
- Legacy LMDB databases from before Moonraker v0.9 can't be imported.
- Out-of-scope components (update manager, power, sensors, MQTT, Spoolman, and others) are
  missing or stubbed.
