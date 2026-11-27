# Capabilities

`server.info` has a `capabilities` object that says what this server's API does. A client
checks a capability instead of comparing `moonraker_version`:

```json
"capabilities": {
  "file_info": {"version": 1},
  "download_content": {"version": 1},
  "file_formats": {"version": 1, "formats": [
    {"name": "gcode", "extensions": [".gcode", ".g", ".gco", ".nc"]},
    {"name": "ufp", "extensions": [".ufp"]},
    {"name": "gcode_3mf", "extensions": [".gcode.3mf"]},
    {"name": "bgcode", "extensions": [".bgcode"]}
  ]},
  "webcams": {"version": 1}
}
```

Moonraker's `server.info` has no `capabilities`. A client that finds none falls back to what
it does for Moonraker, version checks included. No other call is needed, so asking Moonraker
costs nothing and logs nothing.

Each capability has a `version`. It goes up when what the capability enables changes in a way
a client has to know about; additions that a client can ignore don't raise it. A capability
that's missing isn't there.

The lists below say what each capability enables. They aren't strict yet: until clients use
them, a capability may gain a feature without a new version.

## file_info

Each file says whether Orrery prints it, so a client doesn't decide from the extension
(`design/2026-11-27-file-info.md`).

- A `file_info` block on each file in:
  - `server.files.get_directory` and `server.files.list`, with `file_info: true`. Without it,
    listings are as Moonraker's: in a long folder the blocks are much of the reply;
  - `server.files.metadata` and `server.files.metascan`;
  - the `item` of `notify_filelist_changed`, and of the replies of uploads, copies, and moves.
- The block's keys: `printable`, `reason`, `format`, `url`, and `content`, the file's streams,
  each with `kind`, `name`, `url`, and for a thumbnail `width`, `height`, and `size`.
- A `file_info` map, keyed by file name, next to the list in:
  - `server.history.list`, `server.history.get_job`, and `notify_history_changed`;
  - `server.job_queue.status`, the queue's other replies, and `notify_job_queue_changed`.

## download_content

`content=` on a download, `GET /server/files/gcodes/<path>`, picks what it gives:

- `content=gcode`: the gcode Klipper prints. For a 3MF, UFP, or binary gcode, that's the gcode
  read from it, saved as `<name>.gcode`.
- `content=original`: the file as it was uploaded.
- `content=thumbnail-<width>x<height>`: a thumbnail.
- A name the file's `file_info` doesn't list is refused with 400.
- Without `content`, a printable file gives its gcode, and any other gives itself.

## file_formats

The formats this printer reads, with the extensions that pick each one, for a client to know
what it can upload before there's a file to ask about, such as for its file picker's `accept`.

- `formats`: each with its `name`, as `file_info`'s `format` names it, and its `extensions`,
  lowercase, with the dot. An extension can have more than one part, `.gcode.3mf`.
- It follows `orrery.toml`: a reader that's turned off isn't listed, and one added under
  `[reader.<name>]` is ([Readers](configuration/readers.md)).

## webcams

Moonraker's webcam API, which Mobileraker uses only from Moonraker v0.8.0-80:

- `server.webcams.list`, `server.webcams.get_item`, `server.webcams.post_item`,
  `server.webcams.delete_item`, and `server.webcams.test`;
- `notify_webcams_changed`.

Mobileraker doesn't check capabilities yet, so Orrery still tells it it's Moonraker v0.9.0
([`moonraker_version`](configuration/access.md)).
