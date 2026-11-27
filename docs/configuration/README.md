# Configuring Orrery

Orrery's settings go in `orrery.toml`, in the printer's config folder, `~/printer_data/config`.

Orrery also reads `moonraker.conf`, and uses the settings it understands, so a printer that ran
Moonraker works without changes. When both files set an option, `orrery.toml` wins. Write new
settings in `orrery.toml`: some work only there, such as roles for networks, API keys, and
agents.

Moonraker never reads `orrery.toml`, so if you go back to Moonraker, your `moonraker.conf` is as
you left it.

Orrery reads its config when it starts. After you change a config file, restart Orrery:

```
sudo systemctl restart orrery
```

## Pages

| Page | What it covers |
|---|---|
| [Access and logins](access.md) | Roles, trusted networks, API keys, users, proxies, and Moonraker compatibility |
| [Actions](actions.md) | Named steps that buttons, macros, and clients run |
| [Agents](agents.md) | Programs that add features to Orrery, and how Orrery starts them |
| [Buttons](buttons.md) | Buttons on the printer computer's GPIO lines |
| [OctoPrint's API](octoprint.md) | Uploading and printing from PrusaSlicer and other slicers |
| [Spoolman](spoolman.md) | Connecting Orrery to a Spoolman server |
| [The update manager](update-manager.md) | Updates for Klipper, Orrery, and other software, from Fluidd and Mainsail |

The rest of this page explains `orrery.toml`, and lists the options Orrery reads from
Moonraker's sections.

## `orrery.toml`

`orrery.toml` is a [TOML](https://toml.io) file. It uses Moonraker's section and option names,
so you can move a setting from `moonraker.conf` by copying it:

```toml
[server]
port = 7125

[file_manager]
queue_gcode_uploads = true
```

A section with a name, such as `[webcam front]` in `moonraker.conf`, is a table inside a table
in `orrery.toml`:

```toml
[webcam.front]
service = "mjpegstreamer"
stream_url = "/webcam/?action=stream"
```

Values have TOML's types:

- Text goes in quotes: `service = "mjpegstreamer"`.
- Numbers and `true` or `false` don't: `port = 7125`, `enabled = false`.
- Lists go in brackets: `admin = ["192.168.1.0/24", "10.8.0.1"]`.

A value of the wrong type stops Orrery, with an error that names the option. For example,
`port = "7125"` is an error, because Orrery expects a number there.

To read `orrery.toml` from another place, start Orrery with `--orrery-config <path>`, or set
`ORRERY_CONFIG`. Orrery then stops if the file isn't there.

## Moonraker's sections

Orrery reads these options from Moonraker's sections, in either file. They mean what they mean
in [Moonraker's documentation](https://moonraker.readthedocs.io/en/latest/configuration/).
Orrery doesn't use Moonraker's other options, such as `ssl_port`, but it keeps them in
`server.config`, where clients can read them.

### `[server]`

| Option | Default | What it does |
|---|---|---|
| `host` | `0.0.0.0` | The address Orrery listens on. `0.0.0.0` means every network |
| `port` | `7125` | The port Orrery listens on |
| `klippy_uds_address` | `~/printer_data/comms/klippy.sock` | Klipper's API socket |
| `max_upload_size` | `1024` | The largest upload, in MiB |
| `use_xheaders` | `true` | Whether Orrery reads a proxy's `X-Real-Ip` and `X-Forwarded-For`; see [Proxies](access.md#proxies) |

### `[file_manager]`

| Option | Default | What it does |
|---|---|---|
| `enable_config_write_access` | `true` | Whether clients can change files in the config folder |
| `queue_gcode_uploads` | `false` | Whether an uploaded file with "print" checked goes to the job queue |
| `file_system_observer` | `inotify` | How Orrery notices files that change on disk, or `none` |
| `enable_observer_warnings` | `true` | Whether Orrery warns when it can't watch a folder |

### `[job_queue]`

| Option | Default | What it does |
|---|---|---|
| `load_on_startup` | `false` | Whether the queue starts when Orrery starts |
| `automatic_transition` | `false` | Whether the next job starts by itself when one ends |
| `job_transition_delay` | `0.01` | Seconds to wait between jobs |
| `job_transition_gcode` | none | Gcode to run between jobs |

### `[webcam <name>]`

Each section is one webcam. In `orrery.toml`, write `[webcam.<name>]`.

| Option | Default | What it does |
|---|---|---|
| `stream_url` | required | The address of the webcam's stream |
| `snapshot_url` | none | The address of a still picture |
| `service` | `mjpegstreamer` | The kind of stream, such as `webrtc-camerastreamer` |
| `enabled` | `true` | Whether clients show the webcam |
| `location` | `printer` | Where the webcam is |
| `icon` | `mdiWebcam` | The icon clients show |
| `aspect_ratio` | `4:3` | The picture's shape |
| `target_fps` | `15` | Frames per second while printing |
| `target_fps_idle` | `5` | Frames per second otherwise |
| `flip_horizontal`, `flip_vertical` | `false` | Whether to mirror the picture |
| `rotation` | `0` | Degrees to turn the picture: `0`, `90`, `180`, or `270` |

Webcams added from Fluidd or Mainsail are kept in Orrery's database, not in a config file.

### `[machine]`

| Option | Default | What it does |
|---|---|---|
| `provider` | `systemd_dbus` | How Orrery controls services. It's the only one Orrery has |
| `shutdown_action` | `poweroff` | What "shut down" does: `poweroff` or `halt` |

### `[data_store]`

| Option | Default | What it does |
|---|---|---|
| `temperature_store_size` | `1200` | How many temperature readings Orrery keeps for graphs |
| `gcode_store_size` | `1000` | How many lines of the console Orrery keeps |

### `[database]`

| Option | Default | What it does |
|---|---|---|
| `database_path` | `~/printer_data/database` | The folder for Orrery's database, `orrery.redb` |

### `[authorization]`, `[button <name>]`, `[octoprint_compat]`, `[spoolman]`, and `[update_manager]`

These have pages of their own: [Access and logins](access.md), [Buttons](buttons.md),
[OctoPrint's API](octoprint.md), [Spoolman](spoolman.md), and
[The update manager](update-manager.md).

## Orrery's own sections

These work only in `orrery.toml`:

| Section | Page |
|---|---|
| `[authorization.networks]`, `[authorization.keys]`, `[authorization.users]`, `[authorization.proxy_login]` | [Access and logins](access.md) |
| `[moonraker_compat]` | [Moonraker compatibility](access.md#moonraker-compatibility) |
| `[agent.<name>]` | [Agents](agents.md) |
| `[action.<name>]` | [Actions](actions.md) |
| `[setup]` | What `sudo orrery setup` sets up besides the service: [Removing old logs](../install.md#removing-old-logs) |

## Command-line options

You rarely need these, since the defaults fit a standard printer. When you give `-d`, `-c`, or
`--orrery-config` to `sudo ~/orrery/orrery setup`, setup writes them into Orrery's service.

| Option | Default | What it sets |
|---|---|---|
| `-d <folder>` | `~/printer_data` | The printer's data folder |
| `-c <file>` | `config/moonraker.conf` in the data folder | Moonraker's config file |
| `--orrery-config <file>` | `orrery.toml` next to `moonraker.conf` | Orrery's config file |
| `-l <file>` | `logs/orrery.log` in the data folder | The log file |
| `-u <file>` | `comms/moonraker.sock` in the data folder | The unix socket for local tools and agents |
| `-v` | off | More detail in the log |
