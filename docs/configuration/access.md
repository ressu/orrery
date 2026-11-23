# Access and logins

Orrery decides what each client may do. A client gets a role from its network, an API key, or
a user login, and the highest of these counts.

If you came from Moonraker, your `moonraker.conf` keeps working: trusted networks and Fluidd's
API key behave as they did. This page explains the settings Orrery adds in `orrery.toml`.

## Roles

| Role | What it may do |
|---|---|
| `viewer` | Watch the printer: status, files, print history, the job queue, and webcams |
| `operator` | Run the printer: gcode, printing, the job queue, file changes, Spoolman, logs, and Fluidd's and Mainsail's settings |
| `admin` | Everything: changing the config, restarting services, rebooting, shutting down, keys, and backups |

Moonraker has no roles: a client it trusts may do everything. So with only a `moonraker.conf`,
Orrery gives trusted clients `admin`, as Moonraker does.

The unix socket, `~/printer_data/comms/moonraker.sock`, gives `admin` to Orrery's own user and
to root.

## Trusted networks

A network's role is what devices on that network may do without a key or a login. List the
networks for each role in `[authorization.networks]`:

```toml
[authorization.networks]
admin = ["192.168.1.10", "10.8.0.0/24"]
operator = "192.168.1.0/24"
viewer = "192.168.1.134"
```

- A role takes one address or network in quotes, or a list of them.
- A network is written as its first address and its size, such as `192.168.1.0/24`. A single
  device is its address, such as `192.168.1.10`.
- The most specific entry wins. Here, `192.168.1.134` is a `viewer`, although it's inside the
  `operator` network.
- A device on no listed network gets nothing without a key or a login.
- Orrery doesn't look up host names yet, so list addresses.

`[authorization.networks]` replaces Moonraker's `trusted_clients`. With both, Orrery uses the
networks, and its log says it ignores `trusted_clients`. Without `[authorization.networks]`,
each network in `trusted_clients` gets `admin`, as in Moonraker.

## API keys

A key gives a role to a program that can't log in, such as Home Assistant or Mobileraker. List
the keys for each role in `[authorization.keys]`, by name:

```toml
[authorization.keys]
operator = ["homeassistant", "mobileraker"]
viewer = "dashboard"
```

Orrery makes a secret for each new key when it starts. A key name may use letters, digits, `_`,
and `-`. To remove a key, delete its name and restart Orrery: the key stops working.

To see a key's secret, so you can give it to the program, run:

```
~/orrery/orrery key show <name>
```

`<name>` is the key's name, such as `homeassistant`. These commands need Orrery running:

| Command | What it does |
|---|---|
| `orrery key list` | Lists the keys and their roles |
| `orrery key show <name>` | Prints the key's secret |
| `orrery key rotate <name>` | Replaces the key's secret with a new one. The old one stops working |

The secrets are in Orrery's database, not in the config, so a backup of the config folder
doesn't hold them.

Fluidd's API key is a separate key, named `moonraker`. See
[Moonraker compatibility](#moonraker-compatibility).

## Users and logins

Users log in from Fluidd's or Mainsail's login page. Give each user a role in
`[authorization.users]`:

```toml
[authorization]
default_role = "viewer"

[authorization.users]
admin = "alex"
operator = ["sam", "guest"]
```

- A user who logs in but isn't listed gets `default_role`. Without `default_role`, their login
  is refused.
- Without `[authorization.users]`, every local user is `admin`, as in Moonraker. Orrery warns
  about this when it starts.
- A login can raise a role and never lowers one. A user who logs in from an `admin` network
  stays `admin`.
- A user can't have the same name as a key.

### Managing users

An admin can add and delete users in Fluidd, in its settings. The role still
comes from `orrery.toml`. Users change their own passwords in Fluidd too.

From a shell, with Orrery running, use:

| Command | What it does |
|---|---|
| `orrery user list` | Lists the users, with their roles |
| `orrery user add <name>` | Adds a user. It asks for the password |
| `orrery user password <name>` | Sets a user's password. Their sessions end at once |
| `orrery user delete <name>` | Deletes a user. Their sessions end at once |

### Login options

These go in `[authorization]`:

| Option | Default | What it does |
|---|---|---|
| `default_role` | none | The role of a user who isn't in `[authorization.users]`. Without it, their login is refused |
| `force_logins` | `false` | With `true`, networks give no role, so every browser must log in. Keys and the unix socket still work |
| `login_timeout` | `90` | Days before a user has to log in again |
| `max_login_attempts` | none | Failed logins from one address before Orrery stops accepting them for a while |
| `login_lockout` | `15` | Minutes Orrery refuses logins after `max_login_attempts` failures |
| `local_logins` | `true` | With `false`, users can't log in with a password, such as when a proxy logs them in |

`force_logins`, `login_timeout`, and `max_login_attempts` are Moonraker's options, so they also
work in `moonraker.conf`. Unlike Moonraker's, Orrery's lockout ends after `login_lockout`
minutes, and a restart clears it.

## Proxies

A proxy, such as nginx in front of Orrery, passes on each client's address. Orrery reads that
address only from the proxies you trust, so that nobody else can claim another address. List
them in `trusted_proxies`:

```toml
[authorization]
trusted_proxies = ["127.0.0.1"]
```

Without `trusted_proxies`, Orrery trusts no proxy. With only Moonraker's `trusted_clients`,
Orrery trusts the same hosts as Moonraker does. `[server] use_xheaders = false` turns proxy
headers off for everyone.

If a trusted proxy's own address has a role, such as `127.0.0.1` inside an `operator` network,
a request through the proxy without the client's address gets that role. Orrery warns about
this when it starts. To avoid it, give local programs keys, and leave the proxy's address out of
`[authorization.networks]`.

### Logins through a proxy

A proxy can log users in itself, as oauth2-proxy or Caddy's `forward_auth` do, and pass on the
user's name in a header. To use the name, add:

```toml
[authorization.proxy_login]
header = "X-Forwarded-User"
```

- `header` is the header with the user's name. The default is `X-Forwarded-User`.
- Orrery reads the header only from a proxy in `trusted_proxies`, so `trusted_proxies` is
  required.
- The proxy must remove the header when a client sends it. Otherwise anyone could claim any
  name.
- The user's role comes from `[authorization.users]` or `default_role`.
- Users log out at the proxy, not in Orrery.

## Browsers on other sites

Moonraker's `cors_domains` lists the websites whose pages may talk to Orrery, such as a Fluidd
that runs on another host:

```toml
[authorization]
cors_domains = ["https://my.fluidd.example", "http://*.local"]
```

Pages served from an address in `[authorization.networks]` are allowed too.

## Moonraker compatibility

`[moonraker_compat]` holds what Orrery does only so that clients written for Moonraker work.
It's on when Orrery reads a `moonraker.conf`, or when `orrery.toml` has the section.

```toml
[moonraker_compat]
legacy_api_key = "admin"
user_source_fix = true
service_alias = true
remote_methods = true
klipper_shutdown = true
```

| Option | Default | What it does |
|---|---|---|
| `enabled` | `true` | With `false`, all of the options below are off |
| `legacy_api_key` | `"admin"` | The role of Fluidd's API key, `moonraker`. With `false`, there's no such key |
| `user_source_fix` | `true` | Shows local users as Moonraker's, so Fluidd offers to delete them and change their passwords |
| `service_alias` | `true` | Makes `sudo orrery setup` make `moonraker.service` another name for Orrery's service |
| `remote_methods` | `true` | Offers Moonraker's remote methods to Klipper's macros, such as `shutdown_machine`; see [Actions](actions.md#moonrakers-remote-methods) |
| `klipper_shutdown` | `true` | Runs `shutdown_machine` and `reboot_machine` from Klipper as an admin, as Moonraker does. With `false`, Klipper's macros can't shut down or reboot the printer's computer |

Without `legacy_api_key`, Moonraker's `enable_api_key` decides whether the `moonraker` key
exists. In `orrery.toml`, use `legacy_api_key` instead of `enable_api_key`. Once every program
has its own key or login, set `legacy_api_key = false`.
