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

## What clients see

`cube.ufp` stays `cube.ufp`: in the file list, in the print's status, in history, and in the job
queue. Moonraker instead turns it into `cube.gcode`, and removes the UFP file.

A download of `cube.ufp` gives the gcode Klipper prints, so a client's gcode preview works. Add
`content=original` to the query for the file as it was uploaded:

```
/server/files/gcodes/cube.ufp?content=original
```

`content=gcode` asks for the gcode by name. Moonraker ignores `content`, so a client can send it
to either. The gcode is saved as `cube.gcode`, so a program that goes by the extension opens it
as gcode. Until Fluidd asks for the original, its Download button saves the gcode.

A macro can't print `cube.ufp` with `SDCARD_PRINT_FILE`, since Klipper doesn't see the file.
Start the print from a client, the job queue, or `printer.print.start`.

## Adding a reader

A program can read another format. Add a table for it, with its extension and command:

```toml
[reader.bgcode]
extension = ".bgcode"
command = ["/opt/bgcode/bgcode-reader"]
```

| Option | Default | What it does |
|---|---|---|
| `extension` | `.ufp` for `ufp`; needed for any other | The files it reads, with the dot |
| `command` | Orrery's own, for `ufp`; needed for any other | The program to run, and its first arguments |
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
