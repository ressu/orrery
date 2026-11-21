# Authorization: API keys first

**Decision (2026-11-21):** orrery's authorization starts with API keys. Each key is declared in
`orrery.toml`, as an agent is, and has a role: `viewer`, `operator`, `agent`, or `admin`. orrery
generates the secrets and keeps them in its database. With a `moonraker.conf`, orrery works as
before: `trusted_clients`, `cors_domains`, and Fluidd's API key behave as in Moonraker. Agents
that orrery starts need no key. User logins come later.

**Status (2026-11-21):** built, except user logins. What differs from Moonraker is in
`auth-roles` and `no-logins` in `docs/regressions.md`.

Moonraker gives full rights to everything it trusts: whole IP ranges, every logged-in user, and
its one API key. Since 2026-11-20 orrery can reboot the printer and control services, so that
model gives every device on the home network that power. This note keeps Moonraker's behaviour
where clients depend on it, and adds keys with limited rights for everything else.

Scope of this step:

- **In:** keys, roles, networks' roles, trusted proxies, CORS, the `Origin` check, agents, and
  the unix socket.
- **Later:** user logins, JWTs, one-shot tokens, `force_logins`, and LDAP. They're designed in
  [2026-11-21-logins.md](2026-11-21-logins.md).

## Principles

- **Predictable.** The rules come from config, never from what's in the database. orrery logs
  them once at startup. Moonraker breaks this rule: `force_logins` applies only once a real user
  exists (`moonraker/components/authorization.py:402,906`).
- **Compatible with a `moonraker.conf`.** A migrated printer keeps working without new config.
- **Deny by default.** An endpoint that the roles table doesn't list needs `admin`.
- **Roles now, scopes later.** Each role is a set of capabilities inside orrery. Scopes, a
  finer way to grant rights, can be built from the same capabilities later.
- **No secrets in the config folder.** Tools such as printerkeep and Klipper-Backup copy that
  folder to a git repository (see [Where secrets live](#where-secrets-live)).

## Clients and what they need

| Client | How it authenticates with Moonraker | Least role in orrery |
|---|---|---|
| Fluidd | Trusted network, or a user login | `admin` for its settings pages, `operator` for printing |
| Mainsail | Not checked: its source isn't on this machine | Probably the same as Fluidd |
| Home Assistant | API key | `operator`, or `viewer` for a dashboard only |
| aldis | Moonraker URL over HTTP and the websocket; it reads only | `viewer` |
| Slicers | OctoPrint's API with `X-Api-Key` | Out of scope: needs `octoprint_compat` |
| Agents orrery starts | Unix socket | `agent` |
| Agents elsewhere | Websocket | `agent`, with the agent's key |
| `orrery` CLI | Unix socket | `admin`, by its user id |

What the sources show:

- **Fluidd:**
  - It logs in with `access.login`, then sends the token as `access_token` in
    `server.connection.identify` (`fluidd/src/store/socket/actions.ts:206`).
  - For HTTP uploads and downloads, it asks for `access.oneshot_token` and adds it to the URL as
    `?token=` (`src/mixins/files.ts:189,326`). It skips the token on a trusted network.
  - It sends no `Authorization` or `X-Api-Key` header, so it can't use an API key itself.
  - It can show and regenerate one API key (`access.get_api_key`, `access.post_api_key`), and
    add and remove users (`src/store/auth/actions.ts`).
  - Until logins come, Fluidd works only from a network with a role.
- **Home Assistant:** the integration (`moonraker-home-assistant`) uses the `moonraker-api`
  library. That library sends the key as an `X-Api-Key` header on the websocket upgrade
  (`moonraker_api/websockets/websocketclient.py`, `WebsocketClient._run`, read on GitHub at
  `master`). It needs no one-shot token.
- **aldis:** it reads `machine.system_info`, `printer.info`, `printer.objects.list` and
  `query`, and `server.files.roots` (`aldis/src/`). It stops Klipper through systemctl itself,
  not through Moonraker.
- **PrusaSlicer:** it sends `X-Api-Key` to OctoPrint's `/api/files/local`
  (`PrusaSlicer/src/slic3r/Utils/OctoPrint.cpp:525`). orrery has no `octoprint_compat`, so
  slicers can't upload yet, with or without a key.

## Threat model

orrery protects against these:

- **Other devices on the network.** A phone, a TV, or a guest's laptop gets only what a trusted
  network grants, and nothing without one.
- **Web pages in the user's browser.** A page on another site can make the browser send
  requests to the printer. The browser comes from a trusted address, so Moonraker would accept
  them. See [Browser requests](#browser-requests).
- **Other users on the printer's computer.** The database is readable only by orrery's user,
  and on the unix socket another user gets only what a key or an agent table gives it (see
  [The unix socket](#the-unix-socket)).
- **A leaked key.** A key has only its role's rights, so a leaked Home Assistant key can't
  reboot the printer.
- **Guessed keys.** A key has 160 random bits, which can't be guessed by trying.

orrery doesn't protect against these:

- **Processes running as orrery's user, and root.** They can read the database or edit
  `orrery.toml`, so no check inside orrery can stop them.
- **A badly set up proxy.** A proxy whose own address has a role, and that doesn't pass on the
  client's address, gives every caller that role. orrery warns at startup about a trusted
  proxy that has a role (see [Proxies](#proxies)). Securing the proxy is its owner's job.
- **DNS rebinding,** a trick that makes a web page count as the printer's own site (see
  [Browser requests](#browser-requests)).
- **Someone reading network traffic.** orrery has no TLS, so keys cross the network in plain
  text. To protect them, put TLS on the proxy, such as nginx or caddy.

### Rights that amount to admin

Some rights look small but give full control. They need `admin`:

- **Writing to the `config` root.** An `orrery.toml` there can set an agent's `command`, which
  orrery runs. Klipper extensions such as `gcode_shell_command` run programs named in the
  config too.
- **Writing an agent's database namespace.** An agent may act on what it reads there.

## Roles

| Role | What it may do |
|---|---|
| `viewer` | Read status, subscribe to printer objects, list and download gcode files, read history, the job queue, and webcams |
| `operator` | `viewer`, plus gcode, print control, emergency stop, Klipper restarts, gcode file changes, the job queue, history changes, webcams, Spoolman, logs, reading the config root, and frontend settings in the database |
| `agent` | `viewer`, plus what the agent owns (see [Agents](#agents)) |
| `admin` | Everything: writing the config root, services, reboot and shutdown, `server.restart`, keys, database backups, and agents' namespaces |

### Endpoints by role

Each endpoint group needs a capability, and each role is a set of capabilities. An endpoint
that isn't listed needs `admin`.

- **`viewer`:**
  - `server.info`, `server.config`, `server.connection.identify`, `server.websocket.id`,
    `server.temperature_store`, `server.gcode_store`, `machine.system_info`,
    `machine.proc_stats`, `access.info`, and `server.extensions.list`.
  - Klipper's `info`, `objects/list`, `objects/query`, `objects/subscribe`, `gcode/help`,
    `query_endstops/status`, and `bed_mesh/dump_mesh` (`klipper/klippy/`, `register_endpoint`).
  - `server.files.list`, `roots`, `metadata`, `thumbnails`, and `directory` (GET), and
    downloads, in the `gcodes` root.
  - `server.history.list`, `job` (GET), and `totals`; `server.job_queue.status`;
    `server.webcams.list` and `item` (GET); `server.database.list` and `item` (GET) in every
    namespace except agents' namespaces and any that hold secrets (user, 2026-11-21). Keys
    aren't in a namespace at all (see [Where secrets live](#where-secrets-live)).
- **`operator`:**
  - Klipper's `gcode/script`, `emergency_stop`, `pause_resume/*`, `gcode/restart`, and
    `gcode/firmware_restart`; `printer.print.*`, `printer.restart`, and
    `printer.firmware_restart`.
  - File changes in the `gcodes` root: upload, delete, move, copy, zip, metascan, and
    directories. Reading the `logs` and `config` roots.
  - `server.job_queue.*` changes, `server.history.job` (DELETE) and `reset_totals`,
    `server.webcams.*` changes and `test`, `server.logs.rollover`, and `server.spoolman.*`.
  - `server.database.item` (POST, DELETE) in namespaces that no agent owns and orrery doesn't
    reserve, such as `fluidd`. Frontends keep their settings there, from every client.
- **`admin`:**
  - `machine.services.*`, `machine.reboot`, `machine.shutdown`, and `server.restart`.
  - File changes in the `config` root.
  - `server.database.backup` and restore, and writes to agents' namespaces and to `moonraker`.
  - `access.get_api_key`, `access.post_api_key`, and `orrery.access.key.*`.
- **Klipper endpoints not listed,** such as one from a Klipper extension, need `admin`.
- **An agent's endpoints** (`agent.<name>.*`, and names mapped to it with `maps`) need
  `operator` by default, as in Moonraker, where any authorized client can call an agent. The
  agent's table can change that with `call_role` (see [Agents](#agents)).

`server.config` shows the contents of `moonraker.conf` and `orrery.toml`, at `viewer`, because
Fluidd reads it on every connection. That's another reason secrets never go in those files: the
config holds only key names and roles, and the secrets are in the database.

## Keys

Keys are listed by role in `orrery.toml`, as networks are (user, 2026-11-21). A key has
only a role, so a table for each key would be more to write than it holds. An agent's key comes
from its own table, with the agent's `role`:

```toml
[authorization.keys]
operator = ["homeassistant", "mobileraker"]
viewer = "dashboard"

[agent.spoolman]
command = "~/orrery/agents/orrery-spoolman"
role = "agent"   # the default, which a table can raise
```

- **orrery makes the secrets.** On startup it generates one for each key in
  `[authorization.keys]` and each `[agent.<name>]` table that has none. Removing a key from the
  config removes it: orrery deletes the secret on the next start. So the config is the full list
  of keys, and anyone who reads it sees every key and its role. A key in `[authorization.keys]`
  is named as listed, and an agent's key is named `agent.<name>`, such as `agent.spoolman`.
- **A name may be listed under only one role.** A name listed twice, a name that isn't a role,
  and the reserved name `moonraker` stop orrery at startup. The old `[key.<name>]` tables do
  too, with an error that shows the new form. If keys get settings beyond a role later, such as
  scopes, a table per key can come back then. A rotated key's open connections lose its rights.
- **List, get, and rotate.** An admin can list the keys with their roles, read a key again, or
  replace it with a new one. To create or delete a key, edit `orrery.toml`. Listing was added
  on 2026-11-21: the config already shows every key, but a list from the running orrery shows
  what it actually loaded (user, 2026-11-21).
  - Over the API: `orrery.access.key.list`, `orrery.access.key.get {name}`, and
    `orrery.access.key.rotate {name}`, at `admin`. These are orrery's own; Moonraker has nothing
    like them. The list has names and roles, never secrets.
  - In the CLI: `orrery key list`, `orrery key show <name>`, and `orrery key rotate <name>`. The CLI talks to the
    running orrery over the unix socket, so it's `admin` by its user id. It can't open the
    database itself, because redb locks the file while orrery runs.
- **Fluidd's API key is the built-in key `moonraker`,** with the `admin` role. Fluidd's
  `access.get_api_key` and `access.post_api_key` get and rotate it. `enable_api_key` turns it
  on or off, as in Moonraker (`authorization.py:133`), and it's on by default. To turn it off
  once every client has its own key or login, set `enable_api_key = false` in `orrery.toml`'s
  `[authorization]`. From a trusted
  network with `operator`, Fluidd's API key dialog shows an error, because reading the key needs
  `admin`.
- **Format:** Moonraker's, 32 lowercase hex characters, for every key (user, 2026-11-21).
  orrery fills them with 128 random bits; Moonraker uses a UUID without dashes
  (`authorization.py:155`). Clients check the format: Home Assistant's integration accepts
  only 32 letters and digits (`moonraker-home-assistant/.../config_flow.py:106-110`). There's
  no prefix such as `orr_`: it would help secret scanners, but it would also tell anyone who
  sees a key what made it.
- **How clients send a key:** as an `X-Api-Key` header on an HTTP request or on the websocket
  upgrade, or as `api_key` in `server.connection.identify`, as in Moonraker
  (`components/websockets.py:100`).
- **Comparing keys:** orrery looks a key up by the SHA-256 of what the client sent, and compares
  in constant time, so response times don't reveal how much of a key matched.
- **Later restrictions,** such as limiting a key to some addresses, go in the same table.

### Where secrets live

- The secrets are in orrery's database, `<data>/database/orrery.redb`, in a redb table of their
  own. It isn't a namespace, so `server.database.*` can't list or read it at any role.
- The database is outside every file root. Tools that back up the config folder never copy it:
  printerkeep backs up exactly that folder (`printerkeep/docs/naming.md:28`), and
  Klipper-Backup's default is `printer_data/config/*`, pushed to a git repository
  (`Klipper-Backup/.env.example`). Moonraker keeps `moonraker.secrets` in the data folder for the
  same reason (`components/secrets.py:26`).
- An admin can read a key again, so the secrets are stored as they are, not hashed. A database
  backup therefore holds working keys. Backups go to `<data>/backup/database`
  (`crates/orrery/src/server.rs:47`), outside the roots too. Keeping backups safe is a separate
  item for later.
- Restoring a database backup brings back the secrets it had. Keys still follow the config: on
  the next start orrery drops the ones without a table and makes the missing ones.

## Networks

A network's role is what clients from that network may do without a key. Moonraker calls such
clients trusted. orrery keeps that apart from trusting a proxy (see [Proxies](#proxies)): the
two used to share `trusted_clients`, and that mixed them up (user, 2026-11-21).

- **With a `moonraker.conf`:** `trusted_clients` works as in Moonraker, and each network in it
  gets `admin`. Host names in `trusted_clients` are skipped for now, with a warning: Moonraker
  looks them up, and orrery doesn't yet.
- **In `orrery.toml`:** `[authorization.networks]` lists the networks for each role, any of
  `viewer`, `operator`, and `admin`. There are few roles and often several networks, so the
  role comes first (user, 2026-11-21). A role with one network may give it as a string:

  ```toml
  [authorization.networks]
  admin = ["192.168.1.10", "10.8.0.0/24"]
  operator = ["192.168.1.0/24"]
  viewer = "192.168.1.134"
  ```

  It replaces `trusted_clients`, and orrery logs that it ignores `trusted_clients`. An address
  in no network gets nothing without a key.
- **The most specific network decides,** as in a routing table (user, 2026-11-21). So
  `viewer = "192.168.1.134"` inside an `operator` network limits that one device, and
  `admin = "192.168.1.10"` raises another. Two entries for the same network are an error, so
  the role is never ambiguous.
- **A network written with the host part set is the network it's in.** orrery reads
  `192.168.1.5/24` as `192.168.1.0/24`, so it clashes with a `192.168.1.0/24` entry under
  another role. Logs and errors always show the network's first address, `192.168.1.0/24`.
- **Behind a proxy,** orrery judges the address the trusted proxy passes on (see
  [Proxies](#proxies)).
- The old `[authorization.trusted_clients]` table is an error that names the new one.

## Browser requests

A web page on another site can make the user's browser send requests to the printer. A key in
a header is safe from this, because the page can't read or set it. A network's role isn't safe:
the browser's own address is what makes the request trusted. So orrery checks `Origin`, which
Moonraker does only on the websocket:

- A browser sends `Origin` on cross-site requests and on websocket upgrades. A request whose
  `Origin` is neither the address it was sent to (its `Host`) nor in `cors_domains` gets no
  rights from its network. A request with a key still works, and so does a request with
  no `Origin`, such as one from `curl` or Home Assistant.
- Moonraker checks `Origin` on the websocket only (`components/websockets.py:305`). Its HTTP
  endpoints accept arguments in the query string, so a page on another site can send gcode with
  a plain form post. CORS hides the reply from the page, but the request has already run.
- orrery today doesn't check `Origin` on the websocket at all (`crates/orrery/src/web.rs:59`).

`cors_domains` is always checked, including on the websocket. An option to turn the check off
may come later, for a proxy that handles CORS itself.

DNS rebinding is out of scope. It's a trick where a site's name switches to the printer's
address after the page loads, so the page counts as the same site as the printer. Checking the
`Host` header would stop it, but people reach a printer by IP address or by names orrery can't
know, so the check would mostly get in the way.

## Agents

- **Agents orrery starts need no key.** Their process proves who they are: the pid for a
  `command` agent, and the unit for a `systemd_unit` agent (`crates/orrery/src/agents.rs:330`).
- **An agent's key lets it connect from anywhere.** The same agent can run on another machine
  and connect over the websocket with its key. A local agent that sends its key is accepted too,
  so the API is the same everywhere. The key also lets the connection claim the agent's
  configured name, so `check_claim` accepts either the unix socket's checks or a matching key.
- **The `agent` role** is `viewer`, plus what the agent owns:
  - its endpoints, `agent.<name>.*`, and answering calls to them;
  - its database namespace, the one named after the agent, such as `spoolman`;
  - the history fields its table lists in `history_fields`;
  - restarting its own unit, for a `systemd_unit` agent (user, 2026-11-21);
  - the events and names its `maps` give it.
  A table can raise the role, such as `role = "operator"`.
- **Calling an agent** needs `operator` by default (user, 2026-11-21). The admin installs and
  configures each agent on purpose, so its endpoints aren't unknown, and Moonraker lets any
  authorized client call an agent. `call_role` in the agent's table changes it, for an agent
  whose endpoints do something dangerous, such as an aldis agent that flashes firmware:

  ```toml
  [agent.aldis]
  systemd_unit = "aldis-agent.service"
  call_role = "admin"   # only admins may call it; the default is "operator"
  ```

  `call_role` can also be `viewer`, for an agent that only reports. `role` is what the agent
  may do; `call_role` is what a client needs to call it.
- **Gcode can reach an agent past `call_role`.** That's a known gap, to revisit later.
  - An agent can register a remote method with `connection.register_remote_method`
    (`crates/orrery/src/extensions.rs:164`). orrery registers it with Klipper.
  - A Klipper macro can then call it with `action_call_remote_method`
    (`klipper/klippy/extras/gcode_macro.py:99,112`). The call comes from Klipper's connection,
    and orrery can't tell which client sent the gcode that ran the macro.
  - Macros come from the config, which only an admin can write. Running one only needs
    `operator`, though, and the macro passes on its parameters. So a macro that forwards to an
    admin-only agent gives that power to every operator.
  - Options for later: an agent's table lists the remote methods it may register, or
    `call_role` covers remote methods too, so an agent with `call_role = "admin"` can't
    register any. Until then, an admin shouldn't write a macro that calls an admin-only agent.
- **A process running as orrery's user is `admin`,** whatever its role. It could read every key
  from the database anyway, so the `agent` role is no sandbox for it. The role protects when the
  agent connects with its key from elsewhere, or runs as another user (`user =` in its table, or
  `User=` in its unit). Run an agent that takes input from the network, such as Spoolman's, as
  another user.

## The unix socket

- **orrery's own user and root are `admin`.** That's as in Moonraker, which skips authorization
  on its unix socket entirely (`moonraker/common.py:353,438`).
- **orrery sets the socket's mode to `0660`,** for its own user and group, instead of
  depending on the umask. Rights are checked by user id anyway, so an agent that runs as
  another user can connect once that user is in orrery's group. The database gets `0600`, for
  orrery's user only, because it holds the keys; its backups copy that mode.
- **Another user who can connect** gets only what a key or a configured agent grants: the
  `agent` role, for an agent whose table names that user with `user =`. Moonraker would give
  that user full rights. This is a difference, and goes in `docs/regressions.md`.

## Proxies

A proxy such as nginx passes on the client's address in `X-Real-Ip` or `X-Forwarded-For`.

**The rule (user, 2026-11-21): orrery reads those headers only from a trusted proxy.** From
any other connection, it ignores them and judges the connection's own address. Otherwise a
caller could send the header itself and claim to come from a network with a role.

Which hosts are trusted proxies (user, 2026-11-21):

- **`trusted_proxies` set:** exactly the hosts it lists. It's orrery's own option, in
  `orrery.toml`. No setup needs it.
- **`trusted_proxies` not set:** none, so orrery reads no proxy headers. The one exception is
  Moonraker's `trusted_clients` with no `[authorization.networks]`: then its hosts are the
  proxies, as in Moonraker, so a migrated `moonraker.conf` works as before.
- **`[server] use_xheaders = false`:** none. orrery ignores the headers from everyone. That's
  Moonraker's option, on by default (`docs/configuration.md:104`,
  `components/application.py:214,328`).

```toml
[authorization]
trusted_proxies = ["127.0.0.1"]

[authorization.networks]
operator = ["192.168.1.0/24"]
```

How a request from a trusted proxy is judged:

- **With the headers:** by the client's address that the proxy passes on. From
  `X-Forwarded-For`, orrery takes the last address, the one the trusted proxy added. Earlier
  addresses come from the client, which can write anything there.
- **Without the headers:** by the proxy's own address, which may have a role (see below).

Moonraker gets the same result another way. It reads the headers from anyone, then refuses the
request unless the proxy's own address is trusted too (`authorization.py:865-871,915-921`).

**A trusted proxy may have a network's role, and orrery warns about it** (user, 2026-11-21).
A proxy that doesn't pass on the headers makes every caller arrive from the proxy's address,
so every caller gets that address's role. Forbidding the role doesn't work on a printer, where
the proxy and local services such as Mobileraker Companion and `layerlock` all connect from
127.0.0.1: they'd all lose their rights at once. A proxy on another host doesn't have this
problem, and can simply be left out of `[authorization.networks]`.

- **At startup,** orrery warns about each trusted proxy that has a role: "trusted proxy
  127.0.0.1 has the operator role, from network 127.0.0.0/8, so its requests without
  X-Real-Ip or X-Forwarded-For get operator."
- **The first request** from a trusted proxy without the headers is logged too, once per run.
- To have no shared address, use keys for the local services, and leave the proxy's address
  out of `[authorization.networks]`.

With only Moonraker's `trusted_clients`, the proxies are its networks, so these rules
don't apply, and orrery behaves as Moonraker does.

The startup log lists the trusted proxies, says whether they came from `trusted_proxies` or
from `trusted_clients`, and says when there are none.

orrery logs both addresses (`describe_peer`, `crates/orrery/src/web.rs:72`). Securing the proxy
itself is its owner's job.

## Later: users and logins

Logins are designed in [2026-11-21-logins.md](2026-11-21-logins.md). Two decisions from this
note carry over to it:

- **Keys appear in `access.users.list`** (user, 2026-11-21), with the source `orrery`: each
  key in `[authorization.keys]` under its name, and each agent as `agent.<name>`. The
  `moonraker` key is listed too, although Moonraker hides `_API_KEY_USER_`: a key that works
  should show up somewhere (user, 2026-11-21). These users have
  keys, not passwords, so logging in, changing a password, and deleting are refused, with an
  error that points to `orrery.toml`. Fluidd greys out the delete button for any source but
  `moonraker`, which suits keys. Fluidd names only the `moonraker` and `ldap` sources
  (`locales/en.yaml:430,440`), so it shows "User managed by app.general.label.orrery
  authentication" until it learns `orrery`.
- **`access.users.list` needs `admin`,** since it names every key. Moonraker lets any
  authorized client read it.

Until logins are built, `access.login` and the user endpoints return an error that says logins
aren't there yet. `access.oneshot_token` hands out tokens that orrery doesn't check, so Fluidd
on a network with a role can build download links.

## Config and migration

How each of Moonraker's `[authorization]` options (`authorization.py:128-200`) maps over:

| Option | In this step |
|---|---|
| `trusted_clients` | As in Moonraker, with `admin`. `[authorization.networks]` in `orrery.toml` replaces it, with a role per network |
| `cors_domains` | As in Moonraker, and also checked on the websocket and for trusted requests |
| `enable_api_key` | Turns the `moonraker` key on or off |
| `[server] use_xheaders` | As in Moonraker: `false` turns proxy headers off |
| `trusted_proxies` | orrery's own, in `orrery.toml`. Not set: none, or the hosts in `trusted_clients` when `[authorization.networks]` isn't set |
| `force_logins` | Ignored until logins; orrery logs that it ignores it |
| `default_source` | Ignored until logins |
| `max_login_attempts` | Ignored until logins |
| `login_timeout` | Ignored until logins |
| `ldap` | Out of scope |

- The same options in `orrery.toml` win, as everywhere else.
- `orrery-import` brings over Moonraker's API key as the `moonraker` key, which updates
  `import-gaps` in `docs/regressions.md`. Users wait for logins.

## Logging

Authorization events go to orrery's own log, under their own target (`orrery::auth`), so they
can be filtered out or sent to a file of their own later. A separate auth log isn't needed yet.
The printer's computer usually runs from an SD card, and every write wears it, so orrery logs
what helps an admin and leaves out what repeats.

**Never logged:** a secret, whole or in part. A key appears by its name only.

**At startup,** one line for each:

- the networks, each with its role, and whether they came from `[authorization.networks]` or from
  `trusted_clients`;
- the trusted proxies, and whether they came from `trusted_proxies` or from `trusted_clients`;
- the keys, each with its name and role;
- whether the `moonraker` key is on;
- keys that orrery made or dropped because their table was added or removed.

**When something changes:**

- A key is rotated: its name, and who rotated it.
- A connection opens: the role it got, and why, such as the key's name or the network.
  orrery already logs one line per connection (`describe_peer`,
  `crates/orrery/src/web.rs:72`); this adds to that line instead of adding a new one.

**What a refused client is told** (user, 2026-11-21). A frontend shows the message to someone
who may not know the roles, apart from the click that caused it. So it says what the request
would do, who may do it, the client's role, and what that role allows:

> Only an admin can reboot the printer's computer. This connection has the operator role,
> from its network, so it can print but can't change the printer's setup.

- **The action** comes from a table of descriptions, such as "restart the klipper service" or
  "delete files in the config folder". A request the table doesn't describe is named by its
  method: "call server.webcams.test".
- **Where the role came from** is only the kind: "from its network", "from its API key", or
  "as the agent spoolman". The network and the key's name stay out of the message, because
  they tell anyone at the screen how the printer is set up. The log names them, for the admin.

A client with no rights gets Moonraker's 401 "Unauthorized", because frontends react to that
code by asking for a login.

**Refusals and warnings, at most once per source:**

- A request refused for lack of rights: once per address and reason, such as "unknown key" or
  "needs `admin`". After that, orrery counts them and logs a total every hour, while there are
  any. A client that retries in a loop, or someone guessing keys, can't fill the log.
- A trusted proxy that sends no proxy headers: once per proxy. That's the sign of a proxy that
  makes every caller look like itself (see [Proxies](#proxies)).
- Proxy headers from a host that isn't a trusted proxy: once per host.
- An `Origin` that isn't allowed: once per origin.

"Once" means once per orrery run, for now. A problem that lasts days would then show up only
near the start of the log. Later, each of these repeats every N minutes or hours, with a count
of what happened in between, once orrery has a small helper for that. Its logging uses plain
`tracing` today, which has no such limits.

These counts live in memory, so they start again when orrery restarts. HTTP requests that use a
key aren't logged one by one: a key's last use is kept in memory, and `orrery key show` reports
it.

## Crypto and footprint

- For keys: `getrandom` for the secrets, `sha2` for the lookup (orrery already uses it), and a
  constant-time comparison. All pure Rust, with no C code.
- For logins later: `ed25519-dalek` and `pbkdf2`, also pure Rust.
- The size cost gets measured when the code lands.

## Testing

- Conformance cases under `auth/`, run against Moonraker too: `trusted_clients`, `cors_domains`,
  and the `moonraker` key.
- orrery-only cases: key roles, rotation, a key removed from the config, `check_claim` with a
  key, the `Origin` check, proxy headers from trusted and untrusted hosts, and another user on
  the unix socket.

## Open questions

- Mainsail and KlipperScreen aren't checked. They get adjusted later, if they need it.
- An option to turn off the `cors_domains` check.
- The agent permission model, once more agents exist: remote methods that gcode can call
  (see [Agents](#agents)).
