# File tools: readers and processors

**Status (2026-11-27):** being redesigned with the user. A first version is built, with
converters: `orrery-convert` with UFP, uploads, and the file watcher
(`crates/orrery/src/files/convert.rs`, `crates/orrery/src/bin/orrery-convert.rs`, the case
`files/ufp`). It isn't pushed, and it will be reworked into readers, below. Files are stored
as uploaded, with the gcode as a cache (A, below). What a download gives is a proposal. Not yet
tried with Cura on a printer.

Decided so far: readers and processors, with one contract; files stored as uploaded, with the
gcode as a cache; and a `content` parameter on downloads. Not decided: when cached files expire,
and which formats are cheap enough to make on demand.

Orrery gets **tools** for files in the gcodes folder. A **reader** knows one format, and tells
Orrery what to print, what the metadata is, and what the thumbnails are. A **processor**
changes gcode, such as adding object labels.

## Why

- **UFP uploads fail.** Orrery refuses `.ufp` (`docs/regressions.md`, `partial-metadata`). Cura
  sends UFP for its thumbnail: through OctoPrint's API when the server reports the UFP plugin
  (`2026-11-27-octoprint-compat.md`), and reportedly through the Moonraker Connection plugin.
- **Object processing is ignored.** Both of the user's printers set `[file_manager]
  enable_object_processing: True`. Moonraker then runs preprocess_cancellation on each upload.
- **More formats will come.** Klipper prints only gcode, but slicers write others: Cura's UFP,
  sliced 3MF files, and PrusaSlicer's binary gcode. Moonraker can't print binary gcode either,
  so PrusaSlicer users have to turn it off.

## Readers, not converters

The first version had converters. They turned out to be the wrong category (user, 2026-11-27):
most of these tools convert nothing. They read.

| Format | What to print | Metadata | Thumbnails | Converts? |
|---|---|---|---|---|
| Gcode | The file itself | Its comments | Decoded from its comments | No |
| UFP | Taken out of the zip, unchanged | None | The picture, taken out | No |
| Sliced 3MF | Taken out of the zip | `slice_info.config` | One for each plate | No |
| Binary gcode (`.bgcode`) | Decoded from compressed blocks | Its metadata blocks | Several sizes | Yes |

So a reader is one thing for every format: given a stored file, it gives the gcode to print,
the metadata, and the thumbnails, each when the format has it. Orrery's metadata code becomes
the gcode reader. Converting, as for binary gcode, is just how one reader gets its gcode.

"Reader" leaves room for more abstract readers later (user, 2026-11-27): readers that read
what another reader gives, such as the gcode reader on the gcode a UFP reader takes out.

**Processors** are the other kind. They change what's printed, and are optional: object labels,
or klipper_estimator's times. They run on the gcode a reader gives.

## Tools, agents, and built-in code

| | Agent | Tool |
|---|---|---|
| Runs | All the time | Once per file, then exits |
| Talks to Orrery | Over the socket: methods and events | Through files, its arguments, and what it prints |
| Costs while idle | A process | Nothing |
| Fits | Services with state: Spoolman, buttons, updates | Jobs on one file |

Moonraker has the same split inside Python: `metadata.py` runs as a separate process, unpacks
UFP files, and runs "gcode processors", commands with a `{gcode_file_path}` placeholder
(`moonraker/components/file_manager/metadata.py:1270`).

## Where readers run

Reading a format means parsing whatever a client uploaded, so a reader is a risk to the process
it runs in. Orrery's own readers should run outside the server (user, 2026-11-27). For now,
they run inside it: gcode, as today, and UFP. They give their result in the same shape as the
contract below, so moving one out later changes nothing else.

A reader Orrery doesn't have is a program, with the contract below.

**Readers and processors run at a low priority** where they can (user, 2026-11-27). An upload,
or a preview of an older file, while a print runs, must not take the CPU from Klipper. A
program runs with `nice` 19, in the idle I/O class. A reader inside Orrery runs on its own
thread, which Linux lets have its own priority (`setpriority` with the thread's id). They run
one at a time, as metadata does now (`parse_lock`). Klipper's own process isn't touched.

## The contract

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
  "metadata": {"slicer": "PrusaSlicer", "slicer_version": "2.8.0", "estimated_time": 930}
}
```

- **Processors use the same contract** where they can (user, 2026-11-27): the gcode is the
  input, and the new gcode is `gcode`.
- **Every field is optional.** A reader gives what its format has. A processor gives `gcode`,
  and may give `metadata`, such as better print times.
- **No `gcode` from a reader means the input is the gcode,** as for a plain gcode file.
- **Paths are inside the work folder.** Orrery refuses a path that leaves it. Orrery makes the
  folder and removes it afterwards.
- **`metadata` uses Moonraker's field names,** as `server.files.metadata` answers them. Orrery
  keeps the fields it knows and logs the others.
- **Orrery places the results:** the gcode where Klipper prints it, and thumbnails at
  `.thumbs/<name>-<width>x<height>.<ext>`. Clients read each thumbnail's `relative_path`, so
  the names are Orrery's to choose. The first version's `.thumbs/<name>.png` copied Moonraker.
- **Exit status 0 means the output is complete.** Standard error goes to Orrery's log, and is
  the reason given when a tool fails.
- **`version`** lets the contract change later without breaking older tools.

A reader's gcode then goes to the gcode reader, for the metadata the format didn't have, and to
the processors. The first reader's fields win.

The first version's contract was three paths: the input, the gcode to write, and a folder for
one picture. The picture folder was an output too, and it held one picture of one size.

## Placing the results

Decided (user, 2026-11-27). A tool never writes where files are kept. Every tool, Orrery's own
readers inside it included, writes only into its work folder. A short routine of Orrery's, apart
from every tool, then takes what the result lists and puts it in place: the gcode in the cache,
thumbnails in `.thumbs`, and metadata in the database. So a tool can later be shut off from the
rest of the disk, if that's ever needed, and nothing else changes.

The routine trusts nothing in the work folder:

- **Only what the result lists,** at a path inside the work folder. Anything else there is left
  and removed with the folder.
- **Only plain files.** A symlink, or anything else, is refused, and files are opened without
  following links, so a tool can't point Orrery at another file.
- **Thumbnails are checked:** a PNG, JPEG, or QOI file, whose size in pixels is what the result
  says. QOI is saved as PNG, as now.
- **Names are Orrery's.** A result says what a file is, not where it goes, so a tool can't name
  a place.
- **Metadata is only fields Orrery knows,** with the types Moonraker gives them.

To shut a tool off fully, it would also need the input without seeing the gcodes folder: a copy
or a hard link in its work folder, or the open file. That's for when it's needed.

## Messages

A client cares whether a file is supported, not how. Messages say what's supported and where to
change it (user, 2026-11-27): "UFP files aren't supported on this printer: [reader.ufp] in
orrery.toml has enabled = false. See docs/configuration/readers.md." A file a reader can't read
"can't be printed", with the reader's reason.

## Where files are stored

Decided: A, below (user, 2026-11-27). Orrery keeps the uploaded file as it was, and make what's printed from it when it's
needed and cheap, as it is for UFP (user, 2026-11-27). What's made is a cache. Orrery already
keeps one: the metadata in its database and the files in `.thumbs` are made from each gcode
file, and are made again when its size or modified time changes (`has_valid_metadata`). The
gcode to print, and a processor's output, would join it.

What Klipper allows decides a lot. Klipper prints only `.gcode`, `.g`, and `.gco` files under its
gcodes folder, hidden folders too (`klippy/extras/virtual_sdcard.py`, `get_file_list`). It
reports the name it was given in `print_stats.filename`, and clients compare that to the names
they list. So for `cube.ufp`, there are two layouts:

| | A: the upload is the file | B: the gcode is the file |
|---|---|---|
| Clients list | `cube.ufp` | `cube.gcode`, as now |
| The upload | `gcodes/cube.ufp` | `gcodes/.originals/cube.ufp` |
| What's printed | `gcodes/.cache/cube.gcode`, made before a print | `gcodes/cube.gcode`, made when the file arrives |
| Klipper reports | `.cache/cube.gcode`, which Orrery must report as `cube.ufp` in `print_stats`, `virtual_sdcard`, history, and the job queue | `cube.gcode` |
| `SDCARD_PRINT_FILE FILENAME=cube.ufp` in a macro | Fails: Klipper doesn't see the file | Works, with `cube.gcode` |
| Disk | The upload, and the gcode while it's cached | Both, always |
| Download | The upload | The gcode; the upload at another path |

A keeps the upload as the file, and makes the gcode on demand. Its cost is one place in Orrery
that owns the name mapping, used everywhere Klipper's name reaches a client. B changes nothing
for clients, but the gcode always exists, so nothing is made on demand.

Either way, a file is read once when it arrives, for its metadata.

### What clients get

Decided (user, 2026-11-27): Orrery's existing API, with a parameter that works on Moonraker too.
Clients can't say which they want, the upload or the gcode. Fluidd fetches
`/server/files/gcodes/<name>` for its gcode preview, its Download button, and its editor alike
(`fluidd/src/mixins/files.ts:175`), and lists `.ufp` files as gcode files
(`src/store/files/getters.ts:136`). Its preview follows a print by `virtual_sdcard.file_position`,
a byte offset, so it needs the bytes Klipper prints: the cached gcode, after any processor.

- **Moonraker's API gives the gcode.** `GET /server/files/gcodes/cube.ufp` sends the gcode Klipper
  prints, and Orrery makes it first if it isn't cached. Every Moonraker client takes the gcodes
  folder to hold gcode, and the preview works.
- **`content` names one of the file's outputs** (user, 2026-11-27), as a reader's result
  lists them: `gcode`, and `original` for the upload itself. Thumbnails could be named the
  same way later.
- **`content` asks for one,** in the query: `?content=gcode` or `?content=original`. Without
  it, Orrery gives the gcode, for now. The user wants the original to be the default someday,
  so a client that knows Orrery asks for each by name, and the default can change without
  breaking it: Fluidd's preview would ask for `gcode`, and its Download button for
  `original`.
- **It's harmless on Moonraker.** Moonraker's download reads only the path and `Range`
  (`moonraker/components/application.py:806`), and its login reads `token`, so it ignores
  `content` and sends the file it has, which is gcode: it removes a UFP once it's unpacked. The
  same request works on both. `files/upload_download` checks both servers with it.
- **For a gcode file, both are the file.**
- **The metadata says the format,** such as `"format": "ufp"`, so a client knows there's an
  original to offer. Moonraker's clients ignore fields they don't know.
- **The list shows the upload,** with its size. A download's `Content-Length` is the gcode's.
- **Fluidd's Download saves gcode named `cube.ufp`** until it asks for the original. Moonraker
  would have kept only the gcode, so only the name is worse.

### Thumbnails, later

For later (user, 2026-11-27). For now, thumbnails stay as they are: files in `.thumbs`, made
when a file arrives, at the paths its metadata lists.

Orrery gives no thumbnail URL, as Moonraker gives none. The metadata lists `relative_path`, such
as `.thumbs/cube-32x32.png`, relative to the file's folder, and the client makes the URL. Fluidd
joins it to the file's folder, and passes each segment of it through `encodeURIComponent`
(`fluidd/src/mixins/files.ts`, `createFileUrl`). So a query in `relative_path`, such as
`cube.ufp?content=thumbnail-32x32`, reaches the server as a file name with `%3F` in it.
Mainsail and KlipperScreen weren't checked.

- **Paths Orrery owns.** `.thumbs/cube-32x32.png` stays the path, but Orrery serves it from the
  cache, or makes it when it isn't there. To a client it's a file, so no client changes.
  `.thumbs` becomes a cache, with the same expiry as the gcode.
- **Sizes known first.** Fluidd picks a thumbnail by its `size` in bytes, the largest or the
  smallest (`getThumb`), so the metadata lists each one's size before a client asks for it.
  Making them when the file arrives, as now, and keeping their sizes, does that.
- **A query, for clients that know Orrery:** such as
  `/server/files/gcodes/cube.ufp?content=thumbnail&size=64x64`, for any size, made on request.
  Moonraker ignores it, as it ignores `content`.
- **Paths a client encoded wrongly** could be recognized: a request for `cube.ufp%3Fcontent%3D…`
  that names no file, sent to the same handler as the query. A workaround, if a client needs
  it.

### The name mapping

One place, where Orrery talks to Klipper:

- **Starting a print** of `cube.ufp` makes sure `.cache/cube.gcode` exists, and asks Klipper to
  print that.
- **Klipper's status** has `print_stats.filename` and `virtual_sdcard.file_path` mapped back to
  `cube.ufp`, in subscriptions and queries alike.
- **History and the job queue** store only `cube.ufp`, so they need nothing.
- **A cached gcode stays while it's printed.** Cached files expire after a time, such as 14
  days (user, 2026-11-27). That's for later: for now they stay.
- **What leaks:** Klipper's console line `File opened:.cache/cube.gcode`, and a macro's
  `SDCARD_PRINT_FILE FILENAME=cube.ufp`, which Klipper refuses, since it doesn't see the file.
  That's a regression to list.

## Config

Tools are set in `orrery.toml`, like agents:

```toml
[reader.bgcode]
extension = ".bgcode"
command = ["orrery-reader-bgcode"]

[processor.objects]
command = ["preprocess_cancellation"]
timeout = 300
```

- `[reader.<name>]` takes `extension`, `command`, `timeout`, and `enabled`. A table names its
  extension: Orrery doesn't try a reader on every file, since the gcodes folder holds other
  files too, such as `.stl`.
- `[processor.<name>]` takes `command`, `timeout`, and `enabled`.
- Orrery's own readers, gcode and UFP for now, are built in, as its agents are. A table of the
  same name changes them one option at a time, and `enabled = false` turns one off. An upload
  of a `.ufp` with UFP turned off is refused.
- `moonraker.conf` has no tools, since Moonraker has no such options.
  `enable_object_processing` turns on the built-in object processor, once there is one.

The first version had `[converter.<name>]`, and the program `orrery-convert`, which ran
`orrery-convert-<format>` for formats it didn't have, as git runs `git-<command>`. Once
Orrery's readers run outside it, the same naming could work for them.

## UFP

A UFP file is a zip with the gcode at `/3D/model.gcode`, and a picture at
`/Metadata/thumbnail.png`. The reader gives both, unchanged.

## Not now

- **An object processor.** preprocess_cancellation is a Python package. Orrery could run it
  when it's installed, or get its own processor later.
- **klipper_estimator and `[analysis]`.** It fits as a processor, but it isn't in scope.
- **3MF and binary gcode.** Binary gcode would be the first reader that converts, and something
  Moonraker can't do.

## Decided

- **Readers and processors,** not converters (user, 2026-11-27).
- **Orrery's own readers run outside it, eventually,** since parsing uploads is a risk to the
  process. For now they run inside it (user, 2026-11-27).
- **Config:** `[reader.<name>]` and `[processor.<name>]`. One `[tool.<name>]` table would suit
  them, but each kind needs too many options of its own (user, 2026-11-27).

## Open questions

1. **Cheap:** which readers make their gcode on demand, and which keep it? Left until a reader
   that isn't cheap comes (user, 2026-11-27). Taking a file out of a zip is cheap.
2. **Expiry:** how and when cached files are removed. Later; about 14 days.
