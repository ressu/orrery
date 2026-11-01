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

## Planned entries

These differences are already decided in the plan. Each gets an entry when the code that
introduces it lands:

- No TLS: `ssl_certificate_path` and `ssl_key_path` are ignored.
- `moonraker.conf` support is best effort: `[secrets]`, Jinja templating, and full option
  validation come later.
- Legacy LMDB databases from before Moonraker v0.9 can't be imported.
- Out-of-scope components (update manager, power, sensors, MQTT, Spoolman, and others) are
  missing or stubbed.
