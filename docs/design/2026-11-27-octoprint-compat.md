# OctoPrint compatibility

**Status (2026-11-27):** decided with the user, who answered the open questions, and built:
`crates/orrery/src/octoprint.rs`, with the cases in `conformance/cases/octoprint/`. Not yet
tried with PrusaSlicer on a printer.

Orrery gets Moonraker's `[octoprint_compat]`: the small part of OctoPrint's API that slicers use
to send a print. It's built into Orrery (user, 2026-11-27).

## Why

PrusaSlicer has no Moonraker host type. To send a print to a Klipper printer, it uses its
OctoPrint host type, and Moonraker answers as OctoPrint. Both of the user's printers, bluev2 and
pink-0, have `[octoprint_compat]`, and so do nearly all Moonraker installs. Without it, "Send to
printer" in PrusaSlicer fails on Orrery.

## Why it's built in

An agent would be better: Orrery's features are agents when they can be
(`2026-11-15-plugin-interface.md`). This one can't be, for now:

- **The paths are OctoPrint's,** such as `/api/version` and `/api/files/local`. An agent's
  methods are at `/agent/<name>/…`, and `maps` gives it only Moonraker's names.
- **The answers aren't Moonraker's.** OctoPrint's answers are bare JSON, without Moonraker's
  `{"result": …}`. Agents have no way to send one.
- **The upload is a file.** An agent gets JSON over a socket, not a multipart upload.

Nearly every install has it turned on, so it isn't a cost only some users pay. It stays in a
module of its own, `octoprint.rs`, which reaches the rest of Orrery only through the upload and
the calls a client could make itself. If agents can someday serve their own HTTP paths and
uploads, it can move out.

## What clients use

| Client | What it calls |
|---|---|
| PrusaSlicer, and slicers built from it (OrcaSlicer, SuperSlicer) | `GET /api/version`, to test the connection: `text` has to start with `OctoPrint` (`PrusaSlicer/src/slic3r/Utils/OctoPrint.cpp:520`). `POST /api/files/local`, with the fields `print`, `path`, and `file` (`:491`). Both send `X-Api-Key` (`:525`) |
| Cura's OctoPrint plugin | The rest of what Moonraker has: settings, the printer's state and temperatures, the job, gcode commands, printer profiles, and starting a print with the upload's `Location` (`moonraker/components/octoprint_compat.py:33`) |

## What Orrery does

Every path Moonraker has, with the same answers. The conformance cases check them against
Moonraker. Each path gets a role, as Orrery's own paths do
(`2026-11-20-authorization.md`). Moonraker has none: any authorized client can call any of them.

| Path | What it answers | Role |
|---|---|---|
| `GET /api/version` | `{"server": "1.5.0", "api": "0.1", "text": "OctoPrint (Orrery <version>)"}` | `viewer` |
| `GET /api/server` | OctoPrint's version, and `safemode` while Klipper isn't ready | `viewer` |
| `POST /api/login`, `GET /api/currentuser` | The caller (below) | `viewer` |
| `GET /api/settings` | No SD card, no webcam (below), and the UFP plugin | `viewer` |
| `GET /api/job` | Only the state, as Moonraker's: `Operational`, `Printing`, `Paused`, `Error`, or `Offline` | `viewer` |
| `GET /api/printer` | The state, its flags, and the temperatures of the bed and each extruder | `viewer` |
| `GET /api/printerprofiles` | One default profile, with a heated bed if Klipper has `heater_bed` | `viewer` |
| `POST /api/printer/command` | Runs each of `commands` as gcode | `operator`, as `printer.gcode.script` |
| `POST /api/files/local` | Orrery's upload, `/server/files/upload`, with its role for the root: `operator` for `gcodes` | `operator` |
| `POST /api/files/moonraker/<root>/<path>` | `select` with `print`: starts the print, or queues it | `operator`, as `printer.print.start` |

**One upload.** `/api/files/local` is `/server/files/upload` at another path, as in Moonraker:
the same checks, metadata, start or queue, and audit line. Only the `Location` header differs.
It points at `/api/files/moonraker/<root>/<path>`, so Cura can start the print there.

**Roles and keys.** A slicer sends `X-Api-Key`, which Orrery already reads. Give the slicer its
own key with `operator`, in `[authorization.keys]`, and it can upload and print, but not change
settings or update software. Before roles, Moonraker's API key and trusted networks are `admin`,
as everywhere else.

### Where it differs

- **The version says Orrery:** `OctoPrint (Orrery <version>)`, not Moonraker's `OctoPrint
  (Moonraker <version>)` (user, 2026-11-27: honesty is the best policy). PrusaSlicer reads only
  the start.
- **`/api/currentuser` and `/api/login` tell the truth.** Moonraker answers that every caller is
  `_api`, an admin. Orrery names the caller: the user, the key's name, or on a trusted network,
  the caller's own address, which tells it nothing it doesn't know (user, 2026-11-27). The
  answer has `admin: true` only for the `admin` role. PrusaSlicer calls neither. Cura's plugin
  posts to `/api/login` while it has no user name, so the name is never `null`.
- **No webcam.** `/api/settings` says the webcam is off, and Moonraker's webcam options are
  ignored, with a warning when one is set (user, 2026-11-27). A client that shows a modern
  printer's webcam uses Moonraker's API.
- **The UFP plugin, as in Moonraker.** `enable_ufp`, on by default, says OctoPrint's UFP plugin
  is installed, and Cura then uploads `.ufp` files, which Orrery reads
  (`2026-11-27-file-tools.md`). With `[reader.ufp] enabled = false`, the plugin isn't
  reported, and `enable_ufp = true`, set in a config, gets a warning in `server.info`. Until
  the reader was built, the plugin wasn't reported at all.
- **A gcode command that fails is logged.** Moonraker logs it too, and answers `{}`, as
  OctoPrint does. Orrery does the same, and each command gets an audit line, as
  `printer.gcode.script` does.

## Config

`[octoprint_compat]` turns it on, in `moonraker.conf` or `orrery.toml`, with Moonraker's
options:

| Option | Default | What it does |
|---|---|---|
| `enable_ufp` | `true` | Whether Orrery says the UFP plugin is installed. Ignored when `[reader.ufp]` is off, with a warning when it's `true` |

Moonraker's webcam options, `flip_h`, `flip_v`, `rotate_90`, `stream_url`, and `webcam_enabled`,
are ignored, with a warning that names them.

## Tests

- **Conformance cases** for each path, against Moonraker and Orrery, on the fake Klippy:
  `octoprint/` in `conformance/cases`. The upload case uploads, prints, and queues, as PrusaSlicer
  does. The role and key cases are Orrery's own, in `cargo test`
  (`2026-11-25-conformance-and-integration-tests.md`).
- **On a printer:** PrusaSlicer's "Test" and "Upload and print" against bluev2 and pink-0, with
  a key that has `operator`.
