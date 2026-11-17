# Conformance suite

This suite checks a Moonraker-compatible server from the outside, over its websocket and its HTTP API.
Each case runs against a fresh server with its own data directory and a fake Klippy. Upstream
Moonraker is the reference: a case is correct when it passes against Moonraker, and orrery is
correct when it passes the same cases.

## Running

```
just conformance              # against upstream Moonraker
just conformance orrery       # against orrery
just conformance-real         # real-klippy cases against real Klipper
just record                   # re-record snapshots from the reference
```

Runner flags go after the target, for example `just conformance reference --filter printer/ --keep`:

- `--filter <text>` runs only cases whose id contains the text.
- `--transport ws`, `--transport http`, or `--transport unix` runs only one transport.
- `--keep` keeps the data directories of failed runs under `.work/runs/` for debugging. Each one
  holds the server's logs and config.
- `--jobs <n>` sets how many cases run at once (default 4).

## How a case runs

1. The runner makes a data directory with `config/`, `logs/`, `gcodes/`, `database/`, and `comms/`.
2. It writes `moonraker.conf` from `fixtures/moonraker/base.conf`, plus the case's `config:` text.
3. It starts the fake Klippy (or real Klipper), then the server.
4. It opens a websocket and waits until Klippy is ready.
5. It runs the steps in order. The first failing step ends the case.

Cases run once per transport. On the `http` run, `call` steps go over HTTP, using the routes in
`http_routes.json`. On the `unix` run, `call` and `raw_ws` steps go over the unix socket,
`comms/moonraker.sock`. Cases opt in to `unix` by listing it in `transports`. Notifications always come over the websocket. Uploads and downloads always go
over HTTP, as they do in frontends.

The suite's coverage of the methods Fluidd uses is tracked in [coverage.md](coverage.md).

## Writing a case

A case is a YAML file in `cases/<area>/`. Its id is `<area>/<file name>`.

```yaml
name: database post then get
klippy: ready              # script in klippy-scripts/, or `none`
transports: [ws, http]     # the default
steps:
  - call: server.database.post_item
    params: {namespace: test, key: a.b, value: 42}
    expect: {namespace: test, key: a.b, value: 42}
  - call: server.database.get_item
    params: {namespace: test, key: a}
    expect: {value: {b: 42}}
```

Other case fields:

- `wait_ready: false` starts the steps before Klippy is ready.
- `config:` holds extra `moonraker.conf` text. A section here replaces the base section of the
  same name.
- `targets: [orrery]` runs the case against orrery only, for orrery's own features that
  Moonraker doesn't have. Such cases have no recorded snapshots.
- `tags: [real-klippy]` also runs the case against real Klipper. Only tag cases that don't drive
  the fake.
- `known_regression: <id>` marks a known difference listed in `docs/regressions.md`. The case is
  still reported, but its failure doesn't fail the run.
- `ignore: [result.files.*.filename]` lists response paths that vary between runs. They're left out
  of snapshot comparisons.
- `files:` lists files to put in the data directory before the server starts, as
  `{path: text}`. For example, `gcodes/cube.gcode: "G28\n"`.

### Steps

| Step | What it does |
|---|---|
| `call: <method>` | Calls a method with `params`. Checks the result with `expect`, or the error with `expect_error`. |
| `expect_notification: <method>` | Waits up to `within_ms` (default 3000) for a notification whose `params` match `match`. |
| `expect_no_notification: <method>` | Fails if a matching notification arrives within `within_ms` (default 500). |
| `klippy: <action>` | Drives the fake Klippy (see below). |
| `expect_klippy_request: {method, params}` | Checks that Klippy received a matching request. It's skipped with real Klipper. |
| `raw_ws: <text>` | Sends a raw websocket frame and checks the reply with `expect`. It runs on the `ws` and `unix` transports, and is skipped on `http`. |
| `upload: <file name>` | Uploads `content` (text) or `fixture` (a file in `fixtures/files/`) to `/server/files/upload`. `form` adds fields such as `root`, `path`, `print`, or `checksum`. Checks the result with `expect` or `expect_error`. |
| `download: <root>/<path>` | Downloads a file. Checks `expect_status` (default 200) and the body with `expect_body` or `expect_fixture`. |
| `agent: {connect: {name, replies}}` | Opens the test agent: a second connection that identifies as an agent. It answers calls relayed to it with `replies`, by method name, such as `status: {result: {ok: true}}` or `broken: {error: {code: -32000, message: nope}}`. Other methods get a -32601 error. It connects over the unix socket on the `unix` transport, and over the websocket otherwise; `via: unix` or `via: ws` picks one. |
| `agent: disconnect` | Closes the test agent's connection. |
| `expect_agent_received: {method, params}` | Waits up to `within_ms` (default 3000) until the test agent receives a matching relayed call or notification. |
| `sleep_ms: <n>` | Waits. |
| `disk: [<change>, …]` | Changes files in the data folder directly, outside the API, one after another with no wait: `{write: <path>, content: <text>}` (or `fixture:`), `{mkdir: <path>}`, `{move: <path>, to: <path>}`, and `{remove: <path>}`, which also removes a folder. Paths are relative to the data folder. |

A `call` step also takes these fields:

- `retry_ms` repeats the call until the expectation holds.
- `save: {name: path}` stores result values. Later steps use them as `${name}`.
- `only: [ws]` runs the step on one transport only.
- `as_agent: true` sends the call from the test agent's connection.
- `http: {verb, path, query, body}` spells out the HTTP request. Use it for GET requests whose
  params aren't plain values.

Moonraker reports some errors differently per transport. On the websocket, a 404 becomes JSON-RPC
code `-32601` and a 401 becomes `-32602`. HTTP keeps the status code. Use `only` to check both.

Fake Klippy actions:

```yaml
- klippy: {update: {print_stats: {state: printing}}}   # change object fields
- klippy: {gcode_output: "// hello"}
- klippy: {state: {state: shutdown, message: "MCU shutdown"}}
- klippy: restart                                        # drop clients, then startup, then ready
- klippy: disconnect                                     # drop clients, keep the state
- klippy: {remote_method: {method: moonraker_test, params: {result: test}}}
```

### Matching

`expect`, `expect_error`, and `match` are partial: extra keys in the response are fine. Arrays
must match element by element. Numbers compare by value, so `1` matches `1.0`. These patterns are
also available:

| Pattern | Matches |
|---|---|
| `$any` | Any value |
| `$absent` | A key that isn't there |
| `$type:number` | A value of that type: `null`, `bool`, `number`, `string`, `array`, or `object` |
| `$regex:^v\d+` | A string that matches the regular expression |
| `{$contains: [a, b]}` | An array that has a matching element for each item |
| `{$excludes: [a, b]}` | An array that has no matching element for any item |
| `{$exact: true, ...}` | An object with no keys beyond the ones listed |

To match a literal string that starts with `$`, write `$$`.

## Snapshots

`just record` stores the shape of every response from the reference in `snapshots/`: object keys,
value types, and error codes, without the values. Runs against orrery, and runs with real Klipper,
are compared with these shapes. That catches missing, extra, and retyped fields that a case's
`expect` blocks don't check. Re-record after changing a case's calls.

## Fixtures

The fake Klippy serves printer state recorded from real Klipper, so it doesn't drift from real
behaviour.

| File | Contents | How to regenerate |
|---|---|---|
| `fixtures/klipper/printer.cfg` | Moonraker's test printer config, adapted for current Klipper and the Linux host MCU | Edit by hand |
| `fixtures/klipper/klipper.dict` | Data dictionary for the Linux host MCU, so Klipper runs with no hardware | `just klipper-dict` |
| `klippy-scripts/base_printer.capture.json` | Klipper's info, endpoints, printer objects, gcode help, and the answers of read-only endpoints such as `query_endstops/status`, for that config | `just capture` |
| `klippy-scripts/*.yaml` | Fake Klippy scripts: a capture plus a state and gcode rules | Edit by hand |
| `http_routes.json` | Moonraker's method to HTTP route table | `just routes` |
| `fixtures/moonraker/base.conf` | The base `moonraker.conf` | Edit by hand |
| `fixtures/files/` | Files for upload and download steps; see its README | Edit by hand |

`.work/` holds the copied upstream sources, their Python environments, and the run directories.
It isn't tracked.
