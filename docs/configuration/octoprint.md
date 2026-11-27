# OctoPrint's API

PrusaSlicer has no Moonraker printer type, so it sends prints to Klipper printers as it would to
OctoPrint. Orrery answers the part of OctoPrint's API that slicers use, as Moonraker does. This
covers PrusaSlicer, the slicers built from it, such as OrcaSlicer and SuperSlicer, and Cura's
OctoPrint plugin.

To turn it on, add the section to `moonraker.conf` or `orrery.toml`. Most `moonraker.conf`
files have it already:

```toml
[octoprint_compat]
```

## Setting up a slicer

In PrusaSlicer, add a physical printer with the host type OctoPrint. Its address is the
printer's, such as `http://printer.local`, and the API key is one of Orrery's keys.

Give the slicer a key of its own, with `operator`, so it can upload and print, but not change
settings or update software:

```toml
[authorization.keys]
operator = ["prusaslicer"]
```

Restart Orrery, then print the key's secret and paste it into the slicer:

```
~/orrery/orrery key show prusaslicer
```

See [API keys](access.md#api-keys). On a network with a role, the slicer works without a key,
with that network's role.

## What it does

| Path | What it does | Role |
|---|---|---|
| `GET /api/version`, `GET /api/server` | The connection test | `viewer` |
| `POST /api/files/local` | Uploads a file, and prints it with "Upload and print". It's Orrery's own upload, so `[file_manager] queue_gcode_uploads` works too | `operator` |
| `POST /api/files/moonraker/<root>/<path>` | Prints an uploaded file, or queues it while a print runs | `operator` |
| `POST /api/printer/command` | Sends gcode | `operator` |
| `GET /api/printer`, `GET /api/job` | The printer's state, and the bed's and extruders' temperatures | `viewer` |
| `GET /api/settings`, `GET /api/printerprofiles`, `GET /api/currentuser`, `POST /api/login` | What Cura's plugin reads when it connects | `viewer` |

Moonraker lets any client with access use every path. In Orrery, each path needs a role.

## How it differs from Moonraker

- **The version says Orrery.** `/api/version` answers `OctoPrint (Orrery <version>)`, where
  Moonraker says `OctoPrint (Moonraker <version>)`. Slicers check only that it starts with
  `OctoPrint`.
- **`/api/currentuser` and `/api/login` name the caller.** They give the user, the key, or on a
  network with a role, the caller's own address, and `admin: true` only for `admin`. Moonraker
  answers that every caller is an admin named `_api`.
- **No webcam.** `/api/settings` reports the webcam as off. A client that shows the printer's
  webcam uses Moonraker's API instead. Moonraker's webcam options, `flip_h`, `flip_v`,
  `rotate_90`, `stream_url`, and `webcam_enabled`, are ignored, with a warning.
- **No UFP.** Orrery can't unpack UFP files yet, so it doesn't tell Cura that it can, and Cura
  uploads gcode. `enable_ufp = true` is ignored, with a warning.
