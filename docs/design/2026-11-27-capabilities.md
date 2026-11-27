# Capabilities

**Status (2026-11-27):** decided with the user, and built: `crates/orrery/src/capabilities.rs`,
with the case `conformance/cases/server/capabilities.yaml`. The reference for clients is
`docs/capabilities.md`. No client uses it yet.

Clients find out what the server can do by comparing `moonraker_version` with the version that
added a feature. Mobileraker reads cameras from the webcam API only from v0.8.0-80, and Orrery
has to tell it it's Moonraker v0.9.0 to get them. A server that isn't Moonraker has no
Moonraker version to compare, and a feature Orrery adds has none at all. So the server says
what it can do (user, 2026-11-27).

## In `server.info`, not a call of its own

A new call fails on Moonraker. Over the websocket, Moonraker answers -32601 and logs
`JSON-RPC Request Error - Requested Method: …` for each call (`moonraker/common.py:791,886`);
over HTTP, it's a 404 in its access log. A client that asked on each connection would leave a
line in every Moonraker user's log. Every client calls `server.info` when it connects, and a
key Moonraker doesn't have costs nothing there.

## The shape

One entry for each capability, each with its own `version`, so one can change without the
others. Entries can carry what a client needs, as `file_formats` carries its formats.

- **Not `components`.** Components say which modules run, such as `spoolman`. Capabilities say
  what the API does.
- **What each enables is written down** in `docs/capabilities.md`, so a client can build on a
  capability (user, 2026-11-27). Not strictly, until clients use them: a capability can gain a
  feature without a new version until then.
- **No vendor name**, so Moonraker could take it up, as with `file_info`.

## The first ones

- `file_info` and `download_content`: Orrery's additions.
- `file_formats`: the upload side of `file_info`. `file_info` covers files on the printer;
  before an upload there's no file to ask about. Fluidd passes its fixed list as its file
  picker's `accept` (`fluidd/src/components/layout/AppUploadAndPrintBtn.vue:51`). Whether
  browsers take a two-part extension such as `.gcode.3mf` in `accept` isn't tested yet.
- `webcams`: the one Moonraker feature a client checks by version. In the clients we have the
  source of, Fluidd uses the version only to warn about an old Moonraker
  (`fluidd/src/store/server/actions.ts:62`), and Mobileraker only for the webcam API.
  Mainsail and KlipperScreen weren't checked.

## Open questions

- Mainsail's and KlipperScreen's version checks, each a capability if one is found.
- Proposing it to Moonraker and the clients, with `file_info`.
