# Plugins as separate programs

**Decision (2026-11-16):** orrery's plugins are separate programs, called **agents**, that
connect to orrery's socket. orrery won't load plugin code into its own process: no shared
libraries, no WebAssembly, no scripting. The first agent is a Spoolman integration, written in
Rust. Once it works, we decide whether moving existing features (the job queue, webcams, and so
on) out of orrery is worth it on a small board.

Agents can be written in any language. Rust comes first, because orrery is Rust and a small
`orrery-agent` crate makes agents short to write.

This note answers one question: can agents do everything plugins need through Moonraker's agent
API, or do they need more? They need more, but most of it goes in orrery's config, not in the
protocol. Each agent gets an `[agent.<name>]` table in `orrery.toml`. The section says how to run the agent,
and which of Moonraker's built-in names the agent answers under. The printer's owner decides
what an agent may take over; an agent can't claim built-in names by itself. The note also covers
footprint, transport, and the order of work.

## Why separate programs

- **No dynamic loading.** Rust has no stable plugin ABI, and a static musl binary can't load
  shared libraries anyway.
- **Footprint goes only where it's used.** On the printer, orrery uses about 20 MB of memory
  against about 56 MB for an idle Moonraker, and both use about 1% CPU. A feature that runs as an
  agent costs nothing when it isn't installed. A feature compiled into orrery costs every user a
  little, installed or not.
- **Failures stay contained.** An agent that crashes, leaks, or blocks doesn't stop prints or
  the frontend. It can be restarted, or updated, without restarting orrery.
- **The core stays testable.** Agents can only add methods and notifications, never change the
  core's answers. So the conformance suite keeps meaning the same thing whatever agents run.

The cost is one more process per agent, and JSON over a socket instead of function calls. Both
need measuring with the first agent (see [Footprint](#footprint)).

## What Moonraker's agent API already does

Moonraker lets outside programs plug in (`components/extensions.py`,
`docs/external_api/extensions.md`). A program connects to the websocket or the unix socket and
identifies with `type: "agent"`. Then it can:

- call every API a frontend can, including its own printer object subscriptions
- register a Klippy remote method with `connection.register_remote_method`, so a gcode macro can
  call the agent
- send an event with `connection.send_event`, which clients get as `notify_agent_event`
- answer calls that clients make through `server.extensions.request`
- be listed by `server.extensions.list`

orrery should implement all of this as Moonraker does, so existing agents work. The conformance
suite can then check orrery against Moonraker, as it does for everything else.

## Is that enough? Spoolman as the test

Moonraker's Spoolman integration (`components/spoolman.py`) is the most demanding plugin on the
list. Here is each thing it does, and whether an agent can do it with Moonraker's API alone:

| What it does | With Moonraker's agent API |
|---|---|
| Read the `[spoolman]` config section | **Yes:** `server.config` returns every section of the file in `orig` |
| Follow the extruder position to measure filament used | **Yes:** its own `printer.objects.subscribe` |
| Answer the `SET_ACTIVE_SPOOL` macro (`spoolman_set_active_spool`) | **Yes:** `connection.register_remote_method` |
| Keep the active spool id | **Yes:** its own database namespace |
| Talk to the Spoolman server over HTTP | **Yes:** the agent's own HTTP client, which also keeps one out of orrery |
| Answer Fluidd's `server.spoolman.get_spool_id`, `post_spool_id`, and `proxy` | **No:** clients can reach an agent only through `server.extensions.request`, and Fluidd doesn't use it |
| Send `notify_active_spool_set` and `notify_spoolman_status_changed` | **No:** agents can send only `notify_agent_event` |
| Appear as `spoolman` in `server.info` components | **No:** and Fluidd hides its Spoolman panel without it |
| Add `spool_ids` to each history job (`auxiliary_data`) | **No:** history fields come only from components |

Five of the nine work today. The other four all come from one fact: Fluidd talks to Moonraker's
built-in names, never to the agent API. For an agent to stand in for a built-in feature, orrery
has to let it answer under those names.

### The agent section

Every gap in the table is about names: Fluidd calls Moonraker's built-in methods and listens for
its built-in notifications. So the agent's table in `orrery.toml` maps those names to the agent:

```toml
[agent.spoolman]
# How the agent runs: command, systemd_unit, or neither (see "Starting agents").
command = "~/orrery/agents/orrery-spoolman"
# Listed in server.info from startup, so Fluidd shows its panel.
component = "spoolman"
# History fields the agent may set on the current job.
history_fields = ["spool_ids"]

# Public name = the agent's name.
[agent.spoolman.maps]
"server.spoolman" = "agent.spoolman"
"notify_active_spool_set" = "agent.spoolman.active_spool_set"
"notify_spoolman_status_changed" = "agent.spoolman.spoolman_status_changed"
```

**Methods.** An agent registers endpoints under its own prefix, `/agent/<name>/…`, with their
request types, as orrery's own components do: `orrery.agent.register_endpoint {path:
"spool_id", request_methods: ["GET", "POST"]}`. orrery passes each call on to the agent like a
`server.extensions.request` call, with the method name after the agent's prefix, such as
`get_spool_id`, and the request's arguments as `params`. Any agent can do this, configured or not, so ad
hoc agents can still expose whatever they like. orrery names JSON-RPC methods after HTTP paths
(`/agent/spoolman/spool_id` with GET and POST gives `agent.spoolman.get_spool_id` and
`post_spool_id`). That's why one map line covers both: `server.spoolman = agent.spoolman`
rewrites the prefix, so `server.spoolman.proxy` and `POST /server/spoolman/proxy` both reach the
agent's `proxy` endpoint. orrery returns the agent's answer, or its error. A mapped name the agent hasn't registered
returns "not found" while the agent is connected, and "Agent <name> not connected" while it
isn't.

**Notifications.** An agent sends events with `connection.send_event`. Unmapped, they go out as
Moonraker's `notify_agent_event`, so ad hoc agents stay compatible with Moonraker clients. A map
line gives one event a public name, such as `notify_active_spool_set`.

**Components.** `component = "spoolman"` lists the name in `server.info` from the moment orrery
starts, whether or not the agent has connected yet. Fluidd reads `server.info` once when it
connects, so a name that appeared later could leave its panel hidden. When Fluidd calls a mapped
method before the agent is up, it gets an error saying the agent isn't connected. Fluidd already
treats that as Spoolman being offline.

**History fields.** These can't come from the config, because only the agent knows the values as
the print runs. The agent calls `orrery.agent.history_field {name: "spool_ids", value: [3]}` to
set a field on the current job. History saves it as `auxiliary_data` when the job ends, in the
shape Fluidd's history table reads, and never waits on another process to do it.
`history_fields` lists which names an agent may set.

**Dangerous maps.** A map can also point a core name at an agent, such as `server.files.list`.
That's the owner's choice, and sometimes a useful one. orrery logs it and adds a warning to
`server.info`, which Fluidd shows. With no maps, the core answers every core name itself, and
the conformance suite's results don't change.

### Who can use an agent's name

Mapped calls go to whichever connection holds the agent's name. Without a check, any program
that can reach the socket could connect as `spoolman` while the real agent is down, and receive
Fluidd's Spoolman calls. The unix socket tells orrery the process id at the other end. orrery
uses it:

- **`command`:** the name is accepted only from the process orrery started.
- **`systemd_unit`:** only from the unit's main process, which `systemctl show` gives.
- **Neither:** from the first connection that claims it. An optional `user` setting limits it to
  one Unix user.

Over the TCP port there's no process id, so a configured name can't be claimed there. Ad hoc
agents with names not in the config can still connect over TCP, once they've logged in (see
[Transport](#transport)).

### What agents don't need

The earlier draft of this note proposed **hooks**: the core asks a question and waits for the
agents' answers. Spoolman needs none. The one place orrery needs them today is "the upload can't
print now; can anyone queue it?", and that matters only if the job queue becomes an agent. Hooks
are left out until a real agent needs one.

## Footprint

Measured on 2026-11-16, on a desktop (x86_64, glibc), with the agent connected to a real
Spoolman over TLS: `orrery-spoolman` uses 8.9 MB of memory, next to orrery's 14 MB. Its static
aarch64 build is 5.3 MB, against Moonraker's component, which shares Moonraker's Python process.
The numbers on bluev2 are still to measure. TLS uses rustls with the pure-Rust
`rustls-rustcrypto` provider, so the agent has no C dependencies; that provider is a pre-release.

What to measure with the Spoolman agent, on the printer:

- **The agent's memory.** A Rust agent on tokio's single-threaded runtime, built for static
  musl, should use a few megabytes. That's a guess until it's measured.
- **The traffic.** Spoolman subscribes only to the toolhead position, which Klippy reports about
  four times a second. A badly written agent that subscribes to everything would cost much more.
  The `orrery-agent` crate should make narrow subscriptions the easy path.
- **orrery's side.** Each agent is one more connection, about the cost of one more Fluidd tab.

The same numbers decide whether moving existing features out of orrery pays off. For example,
does the job queue cost orrery more memory than an agent process would? That's measured after
Spoolman, not guessed now.

## Starting agents

Agents connect to orrery, so the protocol doesn't care who started them. The agent section
picks one of three ways:

- **`command`:** orrery starts the program and passes it the socket path in an environment
  variable. It restarts the program with a growing delay if it exits, and stops it when orrery
  stops. The program's output goes to orrery's log, marked with the agent's name. This is the
  convenient default, with nothing else to set up.
- **`systemd_unit`:** the agent runs as a systemd unit, such as
  `systemd_unit = "orrery-spoolman.service"` with `systemd_scope = "user"` (or `"system"`). orrery starts
  the unit with `systemctl` if it isn't running, and can restart it. This is the "elaborate"
  setup, with systemd's own restarts, limits, and logs. A user unit needs no extra permissions;
  a system unit needs the same sudo or polkit rule that Moonraker's service control needs.
  These agents can also appear in `system_info`'s service list, so Fluidd shows restart
  buttons. That covers part of the `no-service-control` gap in `docs/regressions.md`.
- **Neither:** something else starts the agent, such as a container or a shell. orrery only
  waits for it to connect.

Agents with no section at all can still connect and register endpoints under `agent.<name>`.
They just can't take built-in names. `server.extensions.list` shows every connected agent,
however it started.

## Transport

Agents on the printer should use the unix socket, not the TCP port. It skips HTTP and the
websocket handshake, and it's framed like Klippy's socket (JSON ending in `\x03`). orrery
doesn't have the unix socket yet (`no-unix-socket` in `docs/regressions.md`), so this work
closes that gap too.

**Logging in.** It depends on the socket:

- **The local unix socket** needs no login. Its file permissions decide who can connect, as in
  Moonraker. This keeps local agents simple.
- **The TCP port**, which other machines can reach, needs one. A remote agent sends an API key
  when it identifies, and orrery refuses agent registrations from a connection without one.
  This waits for authorization; until then, orrery accepts agents only on the unix socket.

**Later: a trusted local socket.** A second local socket, or a short handshake on the first,
could mark a connection as trusted for things ordinary local clients shouldn't do. It isn't
designed yet; the name check in [Who can use an agent's name](#who-can-use-an-agents-name)
covers what Spoolman needs.

## Configuration

**Decision (2026-11-16):** orrery's own config is TOML, in `orrery.toml`. Agent tables and
every later orrery-only setting go there. Moonraker's INI format isn't used for anything new.

- **`moonraker.conf` is read for convenience,** so a Moonraker install keeps working. orrery
  only reads it, never writes new sections into it, and Moonraker never sees `orrery.toml`. So
  switching back to Moonraker stays clean: Moonraker warns about every config section it doesn't
  know, and says a later version will refuse to start.
- **Nobody has to write TOML to get started.** orrery has built-in defaults. Someone moving from
  Moonraker gets a working setup from those defaults plus `moonraker.conf`, with no `orrery.toml`
  at all. A tool that converts `moonraker.conf` into `orrery.toml`, like `orrery-import` does
  for the database, is on the nice-to-have list.
- **`orrery.toml` wins** when both files set the same thing. It sits next to `moonraker.conf`,
  in the config folder, unless an option points elsewhere.
- **Settings can nest.** The INI format has no tables, so lists of pairs such as `maps` needed
  workarounds. TOML has tables and arrays, so orrery uses them.

**Agents and config.** Agents can read their settings from orrery's config or from their own
file; orrery supports both and requires neither. orrery's own agents keep their settings in
`orrery.toml`, and read them with an orrery method that returns the agent's table as typed
JSON. `server.config` stays as Moonraker has it, with `moonraker.conf`'s sections as strings,
so agents written for Moonraker still find `[spoolman]` there. For someone moving from
Moonraker, the Spoolman agent falls back to `[spoolman]` in `moonraker.conf` when `orrery.toml`
doesn't configure it.

## Processors

**Status (2026-11-19):** an idea for later, not built. This section fixes the conventions so
the first processor can follow them.

Some plugin work runs once per file and is then done: reading a file's metadata, turning one
file format into another, or getting a file from somewhere else. That work doesn't need an
agent that stays connected. It needs a **processor**: a program orrery starts for one file,
which then exits. Moonraker already works this way for metadata: it starts `metadata.py` for each
file, and stops it after `default_metadata_parser_timeout`.

| | Agent | Processor |
|---|---|---|
| Runs | All the time; restarted when it exits | For one file, then exits |
| Talks to orrery | The unix socket and the JSON-RPC API | Arguments in, JSON on stdout |
| Memory | About 5 MB, all the time | Only while it runs |
| Good for | Following an outside service, like Spoolman | Reading or changing a file |

A processor never uses orrery's API, so a short Python script can be one.

### Kinds

orrery runs a new file through the processors that match it, in this order:

1. **Fetch** gets a file into a root from somewhere else, such as a URL. It writes to a temporary
   path that orrery gives it, and orrery moves the file into place.
2. **Preprocess** turns the file into another one, such as a sliced 3MF into gcode, or gcode
   with the object labels that cancel-object needs. It writes to an output path, and orrery
   swaps the result in, in one rename.
3. **Extract** reads the file's metadata, and writes thumbnails into a folder that orrery names.
   It prints the metadata as JSON. orrery's own Rust extractor stays the default. If image
   decoding keeps orrery's memory high, that extractor can become a processor program too,
   with nothing else changing.

Preprocess and extract come first, because they share one pipeline. Fetch waits for a real use.

### Finding processors

Users don't set processors up one by one. orrery finds them, as it finds the built-in Spoolman
agent:

- **One folder per processor,** in `processors/` next to the orrery binary. For example,
  `processors/gcode-3mf/` holds the program and a `processor.toml`.
- **The manifest** gives the kind, the file name endings it handles, the command, a timeout, and
  the version. orrery reads manifests without running anything, so finding processors costs
  nothing, and a broken one can't stall startup.
- **Adding the folder turns a processor on,** and removing it turns the processor off. A
  `[processor.<name>]` table in `orrery.toml` is only for turning one off or changing its
  options.
- **Changes are picked up while orrery runs.** The file watcher sees a processor folder appear,
  change, or go away. A later update manager can unpack a new version beside the old one, and
  rename it into place in one step, with no restart.

```toml
# processors/gcode-3mf/processor.toml
kind = "preprocess"
endings = [".gcode.3mf"]
command = ["gcode-3mf"]
timeout = 60
version = "0.1.0"
```

### How they fit what's built

- **Announcements:** a new file goes through its processors before `notify_filelist_changed`
  announces it. The watcher's announcer already waits for metadata, so processors run there too.
  orrery marks the files a processor writes as its own changes, as it does for thumbnails, so a
  processed file gets one announcement.
- **Uploads that start a print** wait until processing ends.
- **Failures:** when a processor fails or runs out of time, orrery keeps the original file, logs
  the reason, and adds one `server.info` warning.
- **Safety:** the same rules as `command` agents: no shell, a timeout, SIGTERM and then SIGKILL,
  and a limit on how much output orrery reads.

### The first example: `.gcode.3mf`

OrcaSlicer can upload a sliced plate as `.gcode.3mf`: a zip file with the plate's gcode, such as
`Metadata/plate_1.gcode`, and PNG previews. A preprocessor unpacks the gcode, and keeps the
preview as the thumbnail. Two things need deciding when it's built:

- **Matching:** orrery's extension check sees only `.3mf` today. Processors match whole name
  endings, so `.gcode.3mf` doesn't match a model `.3mf` that isn't sliced.
- **The original:** keep the `.gcode.3mf` next to the gcode, or replace it.

## The order of work

1. **The unix socket.** It comes first: agents on the printer use it, the name check needs its
   process ids, and it closes the `no-unix-socket` gap.
2. **Moonraker's agent API,** with conformance cases recorded against Moonraker. A test agent in
   the runner registers a remote method, sends events, and answers `server.extensions.request`.
3. **`orrery.toml`, agent endpoints, and agent tables:** reading `orrery.toml` alongside
   `moonraker.conf`, then `/agent/<name>/…` endpoints, maps, components, history fields, and the
   name check, with orrery-only cases.
4. **The `orrery-agent` crate:** connect over the unix socket, identify, register endpoints and
   remote methods, call methods, and receive notifications.
5. **The Spoolman agent,** tested against Fluidd and a Spoolman server, then measured on
   bluev2.
6. **Starting agents** with `command` and `systemd_unit`. `command` is done (case
   `agents/command`). `systemd_unit` waits for service control. The comparison of D-Bus and
   `systemctl` is in `2026-11-19-service-control.md`: D-Bus, with orrery's own client.
7. **Decide what else moves out,** using the measurements from step 5. Decided on 2026-11-19:
   nothing that's built moves out. Webcams are small, need no extra crates, and every frontend
   uses them, so an agent would cost more memory than the code it moves. Files, history, the job
   queue, and process stats are the core that agents build on. Agents are for the Moonraker
   components orrery doesn't have yet, such as `update_manager`, `power`, and `mqtt`.
8. **Processors,** later: preprocess and extract first, with `.gcode.3mf` as the first one (see
   [Processors](#processors)).

Steps 1 to 4 add no plugin features yet, so the conformance suite checks them as they go.

## Open questions

1. **Map layout:** one `maps` table for methods and notifications, as above, or separate
   `methods` and `notifications` tables. Separate tables are easier to check; one is shorter.
2. **The trusted local socket:** a second socket or a handshake, and what it allows. Designed
   when an agent needs more than the name check gives it.
