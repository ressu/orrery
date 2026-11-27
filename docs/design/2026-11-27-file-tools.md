# File tools: converters and processors

**Status (2026-11-27):** a first version is built: `orrery-convert` with UFP, uploads, and the
file watcher (`crates/orrery/src/files/convert.rs`, `crates/orrery/src/bin/orrery-convert.rs`,
the case `files/ufp`). Its contract, below, is being redesigned with the user, together with how
gcode files are stored: [A contract for every tool](#a-contract-for-every-tool) and
[Storing the original file](#storing-the-original-file) are proposals, with open questions.
Not yet tried with Cura on a printer.

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

## The first version's contract

Built, and to be replaced by the one below. A converter gets three paths: the file to read, the
gcode file to write, and a folder where it may write `thumbnail.png`. A processor would get one:
the gcode file to change. The picture folder is an output too, and it takes one picture of one
size. Metadata, with several thumbnails, didn't fit (user, 2026-11-27).

## A contract for every tool

Proposal. Converters, processors, and metadata readers are the same kind of program: one file
in, some results out. They get one contract:

```
<command> <input> <work folder>
```

The tool reads `<input>`, which it must not change, writes its files into `<work folder>`, and
prints one JSON object on standard output that says what it made:

```json
{
  "version": 1,
  "gcode": "model.gcode",
  "thumbnails": [
    {"file": "small.png", "width": 32, "height": 32},
    {"file": "large.png", "width": 300, "height": 300}
  ],
  "metadata": {"slicer": "Cura", "slicer_version": "5.6.0", "estimated_time": 930}
}
```

- **Every field is optional,** and what a tool must give depends on its kind: a converter and a
  processor give `gcode`, and a metadata reader gives `metadata`, `thumbnails`, or both. A
  converter may give the rest too, when its format has them: a UFP has a picture, and
  PrusaSlicer's binary gcode has thumbnails in several sizes and its own metadata blocks.
- **Paths are inside the work folder.** Orrery refuses a path that leaves it. Orrery creates the
  folder and removes it afterwards.
- **`metadata` uses Moonraker's field names,** as `server.files.metadata` answers them. Orrery
  keeps the fields it knows and logs the others.
- **Orrery places the results:** the gcode where it's printed, and thumbnails at
  `.thumbs/<name>-<width>x<height>.<ext>`, the names its metadata reader uses now. Clients read
  each thumbnail's `relative_path`, so the names are Orrery's to choose. That replaces the first
  version's `.thumbs/<name>.png`, which copied Moonraker.
- **Exit status 0 means the output is complete.** Standard error goes to Orrery's log, and is
  the reason given when the tool fails.
- **`version`** lets the contract change later without breaking older tools.

Orrery's own metadata reader is in the server, not a separate program. It returns the same
result inside Orrery, so a file's metadata comes from one place: a converter's results first,
then the metadata reader's on the gcode, filling in what's missing.

Moonraker's `metadata.py` works the same way: a separate process, given a file, that prints one
JSON object (`moonraker/components/file_manager/metadata.py`, `main`).

## Messages

A client cares whether a file is supported, not how. Messages say what's supported and where to
change it, never "converted" (user, 2026-11-27): "UFP files aren't supported on this printer:
[converter.ufp] in orrery.toml has enabled = false. See docs/configuration/converters.md." A
file the tool can't read "can't be printed", with the tool's reason.

## Storing the original file

Proposal (user, 2026-11-27). Orrery keeps the uploaded file as it was, and makes the gcode from
it when it's needed, if that's cheap, as it is for UFP. The gcode is a cache.

What Klipper allows decides a lot of it. Klipper prints only `.gcode`, `.g`, and `.gco` files
under its gcodes folder (`klippy/extras/virtual_sdcard.py`, `get_file_list`), and reports the
name it was given in `print_stats.filename`, which clients compare against file names. It finds
a file in a hidden folder, such as `.cache`, since it looks through every folder for
`SDCARD_PRINT_FILE`. There are two ways to do it:

| | A: the original is the file | B: the gcode is the file |
|---|---|---|
| Clients list | `cube.ufp` | `cube.gcode` |
| The original | `gcodes/cube.ufp` | `gcodes/.originals/cube.ufp` |
| The gcode | `gcodes/.cache/cube.gcode`, made before a print, removed when it's old | `gcodes/cube.gcode`, made when the file arrives |
| Klipper prints | `.cache/cube.gcode`, and Orrery reports `cube.ufp` in `print_stats`, `virtual_sdcard`, history, and the job queue | `cube.gcode`, as now |
| `SDCARD_PRINT_FILE FILENAME=cube.ufp` in a macro | Fails: Klipper doesn't know the file | Works with `cube.gcode` |
| Disk | The original, and the gcode while it's cached | Both, always |
| Download | The original | The gcode, and the original with a new path |

A matches "store the original" best, and saves the most space. It needs Orrery to translate the
cached file's name back everywhere Klipper reports it, which is many places for a client to see
the wrong name, and macros can't start the file by its own name. B keeps everything as now, but
the gcode is always there, so it isn't on demand.

Metadata needs the gcode either way, so the file is converted once when it arrives. With A, the
gcode can then be dropped and made again before a print.

## Open questions

1. **The contract:** one `<input> <work folder>`, with the JSON result, for converters,
   processors, and metadata readers? Or do processors keep changing a file in place?
2. **Storage:** A, B, or something else? With A, when is a cached gcode removed: after the
   print, after some days, or when the disk is low?
3. **Cheap:** which formats are made on demand? UFP is a zip, and fast. A format that's slow to
   convert, or a processor such as object labels, could be kept as gcode instead. Should a
   converter say which it is, such as `cache = true`?
4. **Downloads:** with A, does a download give the original, the gcode, or either, by a
   parameter?

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
