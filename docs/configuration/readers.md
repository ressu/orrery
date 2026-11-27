# Readers

Klipper prints gcode. A reader lets Orrery print a file of another format: it gives Orrery the
gcode to print, the file's metadata, and its thumbnails. Orrery keeps the file as it was
uploaded, and lists it under its own name. Klipper prints the gcode from a cache.

## UFP

Cura's UFP files work without any settings. A UFP file holds the gcode and a picture of the
print, which Fluidd and Mainsail show as the file's thumbnail. Cura sends UFP files through
[OctoPrint's API](octoprint.md), and some Cura plugins send them through Moonraker's.

To turn UFP files off, so an upload of one is refused:

```toml
[reader.ufp]
enabled = false
```

## Sliced 3MF

OrcaSlicer and BambuStudio can export a sliced plate as a `.gcode.3mf` file, which Moonraker
can't print. Orrery prints one without any settings. The plate's pictures are the thumbnails,
and its gcode gives the metadata. The file must hold one sliced plate: export a plate, not all
of them. A 3MF project that isn't sliced, a plain `.3mf`, isn't printable.

Fluidd goes by the part of the name after the last dot, `.3mf`, which it doesn't know as a
file to print. It has no Print button for the file yet, and right after an upload it shows the
file without its thumbnail and details; reload the page to see them. Start the print with
`printer.print.start`, for example:

```
curl -X POST "http://printer.local/printer/print/start?filename=cube.gcode.3mf"
```

To turn sliced 3MF files off:

```toml
[reader.gcode_3mf]
enabled = false
```

## Binary gcode

PrusaSlicer can write binary gcode, `.bgcode`, which is smaller than text gcode and which
Moonraker can't print. Orrery prints one without any settings. It turns the file into the text
gcode PrusaSlicer itself would write, with the file's metadata, and Klipper prints that. The
file's thumbnails are shown as they are, and aren't added to the text.

Fluidd doesn't know `.bgcode` as a file to print, as with sliced 3MF files above: it has no
Print button for it, and shows its thumbnail after a reload.

To turn binary gcode off:

```toml
[reader.bgcode]
enabled = false
```

## What clients see

`cube.ufp` stays `cube.ufp`: in the file list, in the print's status, in history, and in the job
queue. Moonraker instead turns it into `cube.gcode`, and removes the UFP file.

A download of `cube.ufp` gives the gcode Klipper prints, so a client's gcode preview works. Add
`content=original` to the query for the file as it was uploaded:

```
/server/files/gcodes/cube.ufp?content=original
```

`content=gcode` asks for the gcode by name, and `content=thumbnail-32x32` a thumbnail by its
size. Each file's `file_info` lists the names it has, with a URL for each. Moonraker ignores
`content`, so a client can send it to either. The gcode is saved as `cube.gcode`, so a program that goes by the extension opens it
as gcode. Until Fluidd asks for the original, its Download button saves the gcode.

Fluidd's editor shows the gcode too, but its Save can't replace `cube.ufp` with the edited
text: the save is refused, and the file stays as it was. To change the gcode, download it, and
upload it under a `.gcode` name.

A macro can't print `cube.ufp` with `SDCARD_PRINT_FILE`, since Klipper doesn't see the file.
Start the print from a client, the job queue, or `printer.print.start`.

## Adding a reader

A program can read another format. Add a table for it, with its extension and command:

```toml
[reader.gx]
extension = ".gx"
command = ["/opt/gx/gx-reader"]
```

| Option | Default | What it does |
|---|---|---|
| `extension` | `.ufp` for `ufp`, `.gcode.3mf` for `gcode_3mf`, `.bgcode` for `bgcode`; needed for any other | The files it reads, with the dot. It can have parts, such as `.gcode.3mf` |
| `command` | Orrery's own, for `ufp`, `gcode_3mf`, and `bgcode`; needed for any other | The program to run, and its first arguments |
| `timeout` | `300` | Seconds before the file can't be read |
| `enabled` | `true` | `false` turns the reader off |

Orrery runs the command with two more arguments: the file to read, and a work folder. The
program writes what it makes into the work folder, and prints one JSON object:

```json
{
  "version": 1,
  "gcode": "model.gcode",
  "thumbnails": [{"file": "small.png", "width": 32, "height": 32}],
  "metadata": {"slicer": "PrusaSlicer", "estimated_time": 930}
}
```

- `gcode` is needed: the file in the work folder that Klipper prints.
- `thumbnails` are PNG, JPEG, or QOI files in the work folder, with their size in pixels.
- `metadata` uses the field names `server.files.metadata` gives. Orrery keeps the ones it knows.

The program exits with 0 when it's done. Anything else, and the file can't be printed, with
what the program wrote to its error output. An upload of such a file is refused.

The program runs as Orrery's user, at a low priority, so it doesn't slow a print. It gets its
input in the work folder, and must not change it. Orrery puts what it lists in place, and
nothing else.
