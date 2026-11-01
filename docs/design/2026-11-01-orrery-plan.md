<!-- Copied from the approved planning session on 2026-11-01. Later decisions are recorded at the end;
     this file is the source of truth for the plan from now on. -->

# Plan: Moonraker-compatible server in Rust, driven by a conformance suite

## Context

We want to experiment with Moonraker, with per-user state support as the eventual goal. Moonraker is about 41k lines of Python, and its tests (`~/sources/moonraker/tests`) exercise Python internals. They don't test the external API. No reusable Moonraker API test suite exists, and the lightweight HTTP test tools (Hurl, Tavern, Venom, Step CI) don't handle websockets. Real clients use the websocket almost exclusively: Fluidd uses 103 JSON-RPC methods and about 27 `notify_*` types, and uses HTTP only for file upload/download and a `/server/info` probe.

So the order of work is:
1. Build a black-box conformance suite.
2. Prove the suite against upstream Moonraker.
3. Write a Rust server that passes it.

The Rust server must be a drop-in replacement for the core features, so Fluidd, Mainsail and KlipperScreen keep working. It keeps its own datastore, but can import an existing `moonraker-sql.db` (settings, users, history). Config and database compatibility are best effort, and known differences are tracked as regressions. Per-user state, plugins and integrations (Home Assistant, Spoolman and so on) are out of scope for now.

**Decisions made:**
- Language: Rust.
- Test runner: written in Rust and driven by YAML case files.
- Klipper backend in tests: both a fake Klippy and real Klipper, in separate tiers.
- Data model: same as Moonraker's.
- Placeholder name: **`orrery`**. It appears only in the workspace crate names and the binary name, so a later rename is mechanical.

## Repository layout (`~/sources/moonraker-experiment`)

```
Cargo.toml                 # workspace
crates/
  orrery/                  # the server binary
  orrery-proto/            # shared types: JSON-RPC envelopes, Klippy wire framing
  fake-klippy/             # scriptable Klippy unix-socket server (lib + bin)
  conformance/             # test runner (lib + `cargo test` integration harness)
conformance/
  cases/<area>/*.yaml      # test cases, e.g. server/, database/, files/, printer/, auth/, history/, job_queue/
  klippy-scripts/*.yaml    # fake-Klippy behaviour scripts used by cases
  fixtures/                # moonraker.conf variants, gcode files, seed DBs
  snapshots/               # responses recorded from upstream Moonraker (golden files)
justfile                   # just ref-up, just test, just test-real, just record
```

VCS: run `jj git init` in the experiment directory. This matches how `~/sources/moonraker` is managed.

Dev environment (low priority, copied from `~/sources/layerlock/flake.nix`): a `flake.nix` dev shell plus `.envrc` (`use flake`) and `rust-toolchain.toml` (1.93.1 with rustfmt, clippy, rust-analyzer, rust-src). It provides:
- the Rust toolchain from `rust-overlay`, with the musl release targets
- `jujutsu`, `just`
- `qemu`, to run the aarch64/armv7 binaries locally
- `uv` + `python3`, only for running upstream Moonraker and real Klipper in the conformance tiers

`cargo-zigbuild` goes in only if the datastore spike picks SQLite.

## Phase 1: Conformance harness (before any server code)

**Target abstraction.** A `Target` knows how to start a server given a generated `moonraker.conf`, a data directory and a Klippy socket path:
- `reference`: upstream Moonraker, run with `uv run --project ~/sources/moonraker python -m moonraker -d <datapath> -c <conf>`. This is the only place Python is involved.
- `orrery`: our binary.
- Each test class gets a fresh temporary `printer_data/` directory and a random port.

**Transports.** HTTP uses `reqwest`. The websocket uses `tokio-tungstenite`, with a JSON-RPC client that matches responses to requests by id and buffers notifications per method. Notification matching is modelled on `tests/fixtures/websocket_client.py`.

**Case format (YAML).** One file per scenario. A case is an ordered list of steps.
```yaml
name: database post then get
klippy: ready_basic            # fake-klippy script; omit for "no klippy"
transports: [ws, http]         # request steps run once per transport
steps:
  - call: server.database.post_item
    params: {namespace: test, key: a.b, value: 42}
    expect: {result: {namespace: test, key: a.b, value: 42}}
  - call: server.database.get_item
    params: {namespace: test, key: a}
    expect: {result: {value: {b: 42}}}
  - expect_notification: notify_filelist_changed   # websocket only
    within_ms: 2000
    match: {params[0].action: create_file}
  - klippy: push_status {toolhead: {position: [1,2,3,0]}}   # drive the fake Klippy
```
- **HTTP mapping.** Each method is mapped to its HTTP route automatically, using a table generated from Moonraker's `register_endpoint` calls. For example, `server.database.get_item` maps to `GET /server/database/item`.
- **Matchers.** A deep partial match by default, plus `$type:number`, `$regex:...`, `$any`, `$len`, and `ignore:` paths for timestamps and sizes.
- **Steps.** Also `upload` (multipart), `download`, `raw_ws` for malformed JSON-RPC, and `restart_klippy` / `disconnect_klippy`.

**Differential mode.**
- `just record` runs the cases against `reference` and writes normalized responses to `conformance/snapshots/`.
- Against `orrery`, the runner checks both the explicit `expect` blocks and a diff against the snapshot, with ignored paths removed.
- New cases can then be written with little or no `expect`, because the reference supplies the truth.

**fake-klippy (Rust).** It listens on a unix socket and speaks Klippy's framing: JSON terminated by `\x03`, as seen in `moonraker/components/klippy_connection.py:191,218`. It answers `info`, `list_endpoints`, `objects/list`, `objects/query`, `objects/subscribe`, `gcode/script`, `gcode/help`, `gcode/subscribe_output`, `register_remote_method` and `pause_resume/*` from a YAML script with a printer-object state model. A control channel used by test steps can push status deltas, emit gcode responses, enter the `shutdown` or `error` state, or drop the connection.

**Real-Klipper tier** (`just test-real`, tagged cases only).
- Reuse the approach in `tests/fixtures/klippy_process.py` and the assets in `tests/assets/klipper/` (`base_printer.cfg`, `klipper.dict`).
- Run `~/sources/klipper/klippy/klippy.py <cfg> -o /dev/null -d klipper.dict -a <uds> -I <pty>`. This runs Klipper with no hardware.
- This tier checks that fake-klippy's behaviour still matches real Klipper.

**Exit criterion for Phase 1:** the suite is green against `reference` with both the fake and real Klippy.

## Phase 2: Writing test cases, prioritized by real client usage

Extract the list of methods and notifications from `~/sources/fluidd/src/api/socketActions.ts` and `src/store/socket*`, and track it as a coverage checklist (`conformance/coverage.md`). Use the request/response examples in `~/sources/moonraker/docs/external_api/*.md` to seed cases, and `docs/external_api/jsonrpc_notifications.md` for notification shapes.

Tiers, in order:
1. **Connect and boot:** `server.info`, `server.config`, `server.connection.identify`, `server.websocket.id`, `printer.info`, the Klippy ready/shutdown/disconnect notifications and the startup sequence. This is enough for Fluidd to reach its dashboard.
2. **Printer:** `printer.objects.list/query/subscribe`, `notify_status_update` with diffing, `printer.gcode.script/help`, `notify_gcode_response`, `printer.emergency_stop/restart/firmware_restart`, `server.temperature_store`, `server.gcode_store`.
3. **Files:** roots, list, directory, metadata and metascan, thumbnails, upload/download/move/copy/delete/zip, `notify_filelist_changed`, `notify_metadata_update`, the print start/pause/resume/cancel flow.
4. **Database:** `server.database.*`, including backup and restore.
5. **History and job queue:** `server.history.*`, `server.job_queue.*`, and their notifications.
6. **Authorization:** users, login/logout/refresh JWT, API key, oneshot token, trusted clients, CORS, `notify_user_*`.
7. **Machine and other:** `machine.system_info`, `machine.proc_stats` and `notify_proc_stat_update`, services and reboot (stubbed or guarded), webcams, announcements (empty feed), extensions/agents, `machine.update.status` (a stub so frontends don't show errors).

## Phase 3: The `orrery` server

**Stack:**

| Purpose | Crates |
|---|---|
| Runtime, HTTP and websocket | `tokio`, `axum` (built-in ws) |
| JSON and config | `serde` / `serde_json`, `rust-ini` plus a Moonraker-compatible config layer |
| Database | `redb` by default; `rusqlite` if the spike says so |
| Crypto | `ed25519-dalek` for JWTs (Moonraker signs JWTs with EdDSA, per-user keys: `authorization.py:75,570,721`), `pbkdf2` + `sha2` for passwords |
| Misc | `tracing`, `clap`, `notify` (file watching), `sysinfo` |

**Build: no zig unless something needs it.** Copy the setup from printerkeep and layerlock (`~/sources/layerlock/.cargo/config.toml`): `rust-lld` plus Rust's bundled musl for `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf` and `x86_64-unknown-linux-musl`, and a `rust-toolchain.toml`. That works only with no C dependencies, so:
- No TLS in the server for now (Moonraker is normally behind nginx; `ssl_certificate_path` goes in `docs/regressions.md`). This avoids `ring` and `aws-lc`.
- Use `default-features = false` wherever a crate would pull in C code.
- CI and `just` run `cargo build --target aarch64-unknown-linux-musl` to catch a C dependency as soon as one appears.
- Add `cargo-zigbuild` only if the datastore spike picks SQLite.

**Internal architecture,** mirroring the concepts in Moonraker's `server.py` but not its Python structure:
- **Endpoint registry.** One handler per method, served over both HTTP and JSON-RPC, like Moonraker's `register_endpoint`. Klippy endpoints discovered via `list_endpoints` are registered dynamically (`klippy_connection.py:387`).
- **Event bus** for internal events, which also drives `notify_*` broadcasts.
- **Component trait** implemented by the core components: `klippy_connection`, `klippy_apis`, `database`, `file_manager`, `authorization`, `machine` (read-only subset), `data_store`, `job_state`, `job_queue`, `history`, `webcam`, `announcements` (stub), `extensions`. This seam is where a plugin system can attach later.

**Datastore: our own, plus a one-way import from Moonraker.** We don't need byte-compatibility with `moonraker-sql.db`. The API behaviour over `server.database.*` stays compatible.

- **Engine:** decided by a spike at the start of Phase 3. The default is **redb** (pure Rust), which keeps the build free of C dependencies (see Build). We switch to `rusqlite` (bundled) plus `cargo-zigbuild` only if history queries or migrations get awkward.
- **Import** (`orrery import <moonraker-sql.db>`; a host-only tool if the engine is redb). Each item is reviewed and tested on its own. Anything not imported gets an entry in `docs/regressions.md`.

| Source | Contents | Plan |
|---|---|---|
| `namespace_store` | Frontend namespaces (fluidd, mainsail, ...), `webcams`, `announcements`, `moonraker` | Import. Decode type-tag values (`q`/`d`/`?`/`s` + native-endian struct, JSON for lists/dicts, `\x00` = None; `database.py:60-78`) into JSON. |
| `authorized_users` (`authorization.py:79`) | Username, PBKDF2-SHA256 hash plus salt (`authorization.py:496`), Ed25519 `jwt_secret`/`jwk_id`, groups | Import. Existing logins, JWTs and API key keep working. |
| `job_history`, `job_totals` (`history.py:81,120`) | Print history and aggregates | Import. |
| `gcode_metadata` namespace | Metadata cache | Skip; regenerate it by rescanning files. |
| Legacy LMDB (`moonraker.db/`) | Pre-v0.9 format | Out of scope. |

- **Repeated imports are idempotent.** Importing the same database twice leaves the store unchanged. Importing several databases one after another merges them without duplicates.
  - Every imported row is upserted by a natural key: `(namespace, key)`, `username`, `(instance_id, job_id)` for history, and `(provider, field, instance_id)` for totals. Moonraker writes `instance_id` into the history and totals rows, so jobs from different printers can't collide even though `job_id` values overlap.
  - On a key conflict the imported row wins. Rows that aren't in the source database are left alone.
  - `--replace` first wipes everything earlier imports wrote, then imports.
  - `--dry-run` prints the counts that would be inserted, updated and skipped.
  - Each run is recorded in the store's `imports` table: source path, SHA-256, time, and counts.
- The import only reads Moonraker's database, never writes to it. Data flows one way, and from then on our store holds the full state.
- Tests: import cases use seed databases created by upstream Moonraker inside the harness. They cover a repeated import (no change), two different sources (merge), and `--replace`. During experiments, only ever import from a copy of a printer's database.

**File metadata.** Moonraker shells out to `file_manager/metadata.py` (1467 lines of slicer parsers). Port the parsers for PrusaSlicer, SuperSlicer, OrcaSlicer, Cura and Klipper Estimator first, along with thumbnail extraction. The rest falls back to basic fields (size, modified time).

**Explicitly out of scope:** update_manager (status stub only), power, sensors, mqtt, spoolman, simplyprint, octoprint_compat, paneldue, wled, zeroconf, ldap, analysis, notifier, button, gpio, td1, dbus/systemd service control beyond a guarded restart.

## Verification

- `cargo test -p conformance -- --target reference` is green before Rust server work starts.
- `cargo test -p conformance -- --target orrery` tracks progress. The runner prints a pass/fail matrix per tier and transport.
- `just test-real` runs the real-Klipper tier against both targets.
- Manual end-to-end check: serve Fluidd from `~/sources/fluidd` (`npm run dev`) against `orrery` with fake-klippy, then with real Klipper in batch mode. Confirm the dashboard loads, temperatures stream, a file uploads and a simulated print runs.
- Import check: run `orrery import` on a copy of a real printer's `moonraker-sql.db`, start `orrery`, and confirm Fluidd settings, webcams, history and existing logins appear.
- Build check: `cargo build --release --target aarch64-unknown-linux-musl` succeeds with no zig and no C toolchain, unless the datastore spike chose SQLite.
- Hardware: the musl binary runs on a Pi next to Klipper, with Moonraker stopped and the same `moonraker.conf`. Any startup warnings about unsupported config are reviewed against `docs/regressions.md`.

## Risks and open items

- Upstream Moonraker's version is unknown: the checkout isn't a git repo, so record the version used for snapshots. If upstream behaviour changes, re-record the snapshots and review the diffs.
- `struct.pack("q")` uses native byte order. That's fine on little-endian Pi and x86, but should be documented.
- **`moonraker.conf` is best effort.** Support the sections and options for in-scope components, `[include]`, and common installs (start from `tests/assets/moonraker/base_server.conf`). Unknown sections and options produce a warning and are ignored; they never stop the server from starting. Bigger features, such as `[secrets]`, Jinja templating and `confighelper.py`'s full validation semantics, are listed in `docs/regressions.md`.
- **Regressions tracking.** `docs/regressions.md` is the single list of known differences from Moonraker (config, datastore import gaps, stubbed endpoints, TLS). Conformance cases for known regressions are marked `known_regression: <id>`, so they're reported in the results without failing the run.

## Decisions made during implementation

- **2026-11-01: reference version.** The reference is upstream Moonraker v0.11.0 plus commits up to
  `b559b9a81161` (2026-10-21), copied into `conformance/.work/moonraker-src` and installed in
  `conformance/.work/ref-venv`. `~/sources/moonraker` is never modified.
- **2026-11-01: Klipper test fixture.** Moonraker's own `tests/assets/klipper/klipper.dict` comes from
  v0.10 firmware, and current Klipper rejects it; its `printer.cfg` also uses the removed
  `relative_reference_index`. `conformance/fixtures/klipper/` holds a Linux host-MCU data dictionary
  built from Klipper v0.13.0-816-gee2c6fb6b, plus a `printer.cfg` adapted from Moonraker's: Linux pin
  names, TMC drivers and LCD removed, `[display_status]` added. Real Klipper reaches `ready` with it in
  batch mode.
- **2026-11-01: fake Klippy state is recorded, not hand-written.** `fake-klippy capture` records `info`,
  endpoints, all printer objects and gcode help from a real Klipper running that fixture. The result
  is `conformance/klippy-scripts/base_printer.capture.json`, and scripts build on it.
- **2026-11-01: HTTP route table from the reference.** `conformance/tools/dump_routes.py` loads the
  reference server's components and dumps its endpoint table to `conformance/http_routes.json`
  (85 methods, 5 of them websocket-only). Klippy-provided methods follow Moonraker's remote rule:
  `printer.a.b` is `/printer/a/b`, GET or POST.
- **2026-11-01: the runner is a binary.** It's `cargo run -p conformance -- --target reference|orrery`
  rather than a `cargo test` harness, so it can take flags (filter, record, keep work dirs) and print a
  results matrix.
- **2026-11-01: Moonraker behaviours the suite pinned down.** orrery must copy these:
  - On JSON-RPC, a 404 error becomes code `-32601` and a 401 becomes `-32602`. HTTP keeps the
    status code (`common.py`, `JsonRPC.execute_method`).
  - `printer.restart` sends `gcode/script` with `RESTART`; it doesn't use Klippy's `gcode/restart`
    endpoint.
  - `server.info` reports `klippy_state: ready` as soon as Klippy's `info` says so, which is
    slightly before Moonraker subscribes to `webhooks`. The runner waits for that subscription too,
    because a state change that arrives earlier reaches nobody.
- **2026-11-01: real-Klipper tier.** Cases tagged `real-klippy` run against `klippy.py` from
  `conformance/.work/klipper-src`. Their response shapes are compared with the snapshots recorded
  using the fake, which checks that the fake still matches real Klipper.
- **2026-11-01: build check.** With the nix dev shell, `cargo build --release --target
  aarch64-unknown-linux-musl` and `armv7-unknown-linux-musleabihf` work with only `rust-lld`.
- **2026-11-01: orrery's command line is free to differ.** The stub accepts Moonraker's flags for
  now, but matching them isn't a requirement; change them as needed.
- **2026-11-01: Phase 2 coverage is tracked in `conformance/coverage.md`**, listing every method,
  notification, and HTTP route Fluidd uses.
- **2026-11-01: more Moonraker behaviours the suite pinned down.** orrery must copy these:
  - A Klippy request that returns `{}` is reported to the client as `"ok"`.
  - Over HTTP, `GET /printer/objects/query` with no query string returns an empty `status`. Over
    the websocket, the same call with no `objects` is a 400 error.
  - Errors other than 404 and 401 keep their HTTP status as the JSON-RPC code, for example 403
    for a write to the protected `moonraker` namespace.
  - Writing a database key below a plain value replaces the value with an object.
  - The job queue starts paused. Pausing it again sends no notification.
  - The `gcodes` root lists only gcode files, but directory listings include `.thumbs`.
  - An upload that replaces a file is still reported as `create_file`.
  - Thumbnail paths in `server.files.thumbnails` are relative to the `gcodes` root.
  - A file with no slicer comments gets `slicer: Unknown`.
- **2026-11-01: runner isolation fixes.**
  - Each server gets its own `TMPDIR`. Moonraker names upload temp files after the current second
    of the monotonic clock, so servers running side by side overwrote each other's uploads.
  - The reference processes run with `GIT_CEILING_DIRECTORIES` set to `conformance/.work`. Since
    the project became a git repository, Klipper otherwise reported our commit as its version.
  - The runner waits for the `print_stats` subscription as well as `webhooks` before a case
    starts, because job tracking misses changes that arrive earlier.
- **2026-11-01: a Moonraker race the suite works around.** Right after `server.files.post_directory`,
  a copy into the new folder can hang for minutes, because Moonraker waits for an inotify event
  from a watch that isn't in place yet. `files/directories` waits one second after creating the
  folder. orrery shouldn't need that wait.
- **2026-11-01: the fake answers read-only endpoints from the capture.** `fake-klippy capture` also
  records `query_endstops/status` and `bed_mesh/dump_mesh`, and scripts can override endpoint
  answers with `responses:`.
- **2026-11-01: datastore spike done; recommendation is redb.** See
  [2026-11-01-datastore-spike.md](2026-11-01-datastore-spike.md). redb was faster on committed
  writes (about 4x on NVMe) and kept history lists flat at 20,000 jobs with a hand-kept index. It
  adds 0.98 MB to the aarch64 binary against 1.73 MB for SQLite, and it needs no C cross
  compiler. Moonraker's database is read for import by a ~300-line pure-Rust SQLite reader, which
  matched Python's `sqlite3` row for row on a database built by upstream Moonraker. Still to
  check: the same numbers on a Pi's SD card.
