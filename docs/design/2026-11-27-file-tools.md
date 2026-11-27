# File tools: converters and processors

**Status (2026-11-27):** decided with the user. The first step is built: the contract,
`orrery-convert` with UFP, uploads, and the file watcher (`crates/orrery/src/files/convert.rs`,
`crates/orrery/src/bin/orrery-convert.rs`, the case `files/ufp`). Processors are designed, not
built. Not yet tried with Cura on a printer.

Orrery gets **tools** for gcode files: programs it runs on one file, which then exit. A
**converter** turns another format into gcode, such as Cura's `.ufp`. A **processor** changes a
gcode file in place, such as adding object labels. UFP is the first converter, and it comes
with Orrery.

## Why

- **UFP uploads fail.** Orrery refuses `.ufp` (`docs/regressions.md`, `partial-metadata`). Cura
  sends UFP for its thumbnail: through OctoPrint's API when the server reports the UFP plugin
  (`2026-11-27-octoprint-compat.md`), and reportedly through the Moonraker Connection plugin.
- **Object processing is ignored.** Both of the user's printers set `[file_manager]
  enable_object_processing: True`. Moonraker then runs preprocess_cancellation on each upload.
- **More formats will come.** Gcode files are the one format Klipper prints, but slicers write
  others, and post-processing tools such as klipper_estimator, which Moonraker's `[analysis]`
  runs, change gcode after the slicer.

## Tools, agents, and built-in code

| | Agent | Tool |
|---|---|---|
| Runs | All the time | Once per file, then exits |
| Talks to Orrery | Over the socket: methods and events | Through files, its arguments, and its exit status |
| Costs while idle | A process | Nothing |
| Fits | Services with state: Spoolman, buttons, updates | Jobs on one file |

A tool needs no API and no connection, so any program can be one, in any language, or a script
around one. Moonraker has the same split inside Python: `metadata.py` runs as a subprocess, it
unpacks UFP files, and it runs "gcode processors", commands with a `{gcode_file_path}`
placeholder (`moonraker/components/file_manager/metadata.py:1270`).

## How a file goes through

1. **The upload lands in a temporary file,** as now.
2. **A converter, if its extension has one:** it reads the upload and writes gcode to a second
   temporary file. The result is named after the upload, with `.gcode`: `cube.ufp` becomes
   `cube.gcode`, as in Moonraker.
3. **The processors, in their order in the config:** each changes the temporary gcode in place.
4. **The file moves into place,** and Orrery reads its metadata, then starts or queues the
   print, as now.

A converter that fails fails the upload, with the tool's message. A processor that fails is
logged, and the file goes on without it, as in Moonraker. Each tool has a timeout. A file that
appears in the gcodes folder by itself, through the file watcher, goes through the same steps:
a `.ufp` copied in is converted, and then removed.

## The contract

A tool is a command, run as Orrery's user, in the data folder. Its output goes to Orrery's log.

- **A converter** gets three arguments: the file to read, the gcode file to write, and a folder
  for the file's picture. It may write the picture there as `thumbnail.png`, and Orrery moves
  it to `.thumbs/<name>.png` next to the gcode. It exits with 0 when the gcode is complete.
- **A processor** gets one argument: the gcode file to change. It exits with 0 when it's done,
  whether or not it changed anything. It must recognize a file it processed already, as
  preprocess_cancellation does, since a file can be uploaded again.

Orrery passes the paths, so a tool never guesses where to write, and never sees the folder
while a print reads from it.

## Config

Tools are set in `orrery.toml`, like agents:

```toml
[converter.bgcode]
extension = ".bgcode"
# command defaults to ["orrery-convert", "bgcode"], which runs orrery-convert-bgcode

[converter.special]
extension = ".foo"
command = ["/opt/foo/foo2gcode"]

[processor.objects]
command = ["preprocess_cancellation"]
timeout = 300
```

- `[converter.<name>]` takes `extension`, `command`, and `timeout`. Each format needs its table,
  which names its extension: Orrery doesn't try a converter on every file it sees, since the
  gcodes folder holds other files too, such as `.stl`. The command defaults to
  `orrery-convert <name>`.
- `[processor.<name>]` takes `command`, `timeout`, and `enabled`.
- Orrery's own converters, UFP for now, are built in, as its agents are. A table of the same
  name changes them one option at a time, and `enabled = false` turns one off. An upload of a
  `.ufp` with UFP turned off is refused.
- `moonraker.conf` has no tools, since Moonraker has no such options.
  `enable_object_processing` turns on the built-in object processor, once there is one.

## `orrery-convert`

Converters are a program of their own, `orrery-convert`, next to `orrery` (user, 2026-11-27).
`orrery-convert <format> <input> <output> <pictures>` converts the formats it has itself, UFP
for now.
For any other format, it runs `orrery-convert-<format>` with the same paths, from its own folder
first, then from `PATH`, as git runs `git-<command>`. A complex converter, in any language, plugs
in by that name. A converter by any other name gets its own `command`.

Memory isn't the reason. A subcommand of `orrery` wouldn't load the whole program: Linux reads
only the pages that run, and they'd be shared with the running Orrery. `orrery --version` peaks
at 5.2 MB resident, of an 11.9 MB program, against 2.2 MB for `true` (x86_64, 2026-11-27). The
reasons are that a converter needs none of the server's code, so a bug in one can't touch it,
and that the naming lets converters be added without changing Orrery.

## UFP

A UFP file is a zip with the gcode at `/3D/model.gcode`, and a picture at
`/Metadata/thumbnail.png`. `orrery-convert ufp <input> <output> <pictures>` writes the gcode,
and the picture as `thumbnail.png`.

Orrery puts the picture at `.thumbs/<name>.png`, as Moonraker does, and its metadata finds it
there for a Cura file, with a 32x32 copy, as Moonraker's does (user, 2026-11-27: good enough for
now; where pictures go isn't fully explored). Putting it in the gcode's header, as PrusaSlicer
writes thumbnails, was the other choice.

## Not now

- **An object processor.** preprocess_cancellation is a Python package. Orrery could run it
  when it's installed, or get its own processor later. Either way, it's a tool.
- **klipper_estimator and `[analysis]`.** It fits as a processor, but it isn't in scope.
- **Other formats,** such as PrusaSlicer's binary gcode, once there's a converter for them.
- **Processors.** They follow the converters' pattern: `orrery-process <name> <file>`, which
  runs `orrery-process-<name>` for the ones it doesn't have (user, 2026-11-27).

## Decided

- **Programs:** `orrery-convert`, and later `orrery-process`, with git's naming for more. There's
  no strict need for a separate program: it's for adding converters easily, and may be more than
  needed (user, 2026-11-27).
- **Config:** `[converter.<name>]` and `[processor.<name>]`, as a first try. One `[tool.<name>]`
  table would suit them, but each kind needs too many options of its own (user, 2026-11-27).
