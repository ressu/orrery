# Known differences from Moonraker

This file lists every known way orrery behaves differently from Moonraker, so the differences are
deliberate and tracked. Each entry has an id. A conformance case that fails because of an entry is
marked `known_regression: <id>`. To fix a difference, remove its entry and the marks together.

Most missing features are simply not written yet. Those show up as failing cases in the
conformance results, not here. This file lists the differences that are deliberate, or that
change how safe or complete orrery is to run.

## Entry format

```markdown
### <id>: <short title>

- **Area:** config, datastore import, API, or build
- **Moonraker:** what Moonraker does
- **orrery:** what orrery does instead
- **Why:** the reason, such as too much effort for now or out of scope
- **Cases:** conformance case ids marked with this regression
```

## Entries

### no-logins: logins come from local users and proxies, and work a little differently

- **Area:** API
- **Moonraker:** users log in with a name and password, and get JWTs. `access.oneshot_token`
  gives tokens for HTTP links. `force_logins` makes trusted clients log in too, and `ldap`
  checks passwords against an LDAP server.
- **orrery:** local users log in with Moonraker's tokens, and a trusted proxy can name a user
  it logged in, with `[authorization.proxy_login]`. Roles come from `orrery.toml`'s
  `[authorization.users]`; see `docs/design/2026-11-21-logins.md`. What's missing or different:
  - Login sources that are agents, such as LDAP, aren't built yet. `access.login` with another
    `source` than `moonraker` is refused, and `[ldap]` is ignored.
  - A proxy user has the source `proxy`, isn't stored, and isn't in `access.users.list`. They
    log out and change their password at the proxy, so orrery refuses both with a message
    that says so.
  - `force_logins` applies whenever it's set. Moonraker applies it only once a user exists.
  - After `max_login_attempts` failed logins, an address is locked out for `login_lockout`
    minutes, 15 by default, with 401 "Too many failed logins from this address. Try again in
    15 minutes." Moonraker's lockout lasts until a restart, with 401 "Unauthorized, Maximum
    Login Attempts Reached".
  - A failed login says "Wrong user name or password", with 401, whatever was wrong. Moonraker
    says "Unregistered User" or "Invalid Password", with 400, which tells a guesser which
    names exist.
  - A login adds rights and never takes any away: the connection gets the higher of its
    network's role and its user's. After a logout, a websocket goes back to its network's
    rights; in Moonraker it has none until it identifies again.
  - `access.post_user` and `access.delete_user` need `admin`; Moonraker allows any authorized
    client. A user who isn't listed in `[authorization.users]` gets `default_role`, or can't
    log in. Without the list, every local user is `admin`, as in Moonraker.
  - `access.users.list` also lists the keys, with source `orrery`, the `moonraker` key
    among them. Moonraker lists only users, and hides its API key user.
  - Changing a password ends the user's other sessions. Moonraker keeps their tokens valid.
  - orrery calls local users' source `local`. Moonraker's replies say `moonraker`, as Moonraker
    does, unless `[moonraker_compat] user_source_fix = false`. Moonraker's API key is set by
    `[moonraker_compat] legacy_api_key`, which can also give it a lower role; `enable_api_key` in
    `moonraker.conf` is its default.
  - Passwords are hashed with Argon2id. Moonraker's PBKDF2 hashes are checked, and replaced at
    the next login.
- **Why:** see `docs/design/2026-11-21-logins.md`.
- **Cases:** `auth/login` (both servers); `auth/users`, `auth/compat`, `auth/oneshot`,
  `auth/lockout`, `auth/force_logins`, and `auth/proxy_login` (orrery only); and `access/trusted`.

### auth-roles: rights come in roles, and some requests need more than Moonraker asks

- **Area:** API
- **Moonraker:** every trusted client, logged-in user, and its one API key has full rights.
- **orrery:** each client has a role, `viewer`, `operator`, or `admin`, and each endpoint needs
  one; see `docs/design/2026-11-20-authorization.md`. With a `moonraker.conf`, trusted networks
  and the API key are `admin`, so they work as before. The differences:
  - Keys are listed by role in `orrery.toml`'s `[authorization.keys]`, and each agent table
    has a key. orrery makes the secrets. `access.get_api_key`, `access.post_api_key`, and
    `access.users.list` need `admin`; Moonraker allows any authorized client.
  - Fluidd's Authentication settings load the user list and the API key, which need `admin`.
    For an operator or viewer, those calls are refused; see `fluidd-auth-nonadmin` in
    `docs/frontend-issues.md`.
  - A browser request from a network with a role whose `Origin` is neither its own `Host` nor in
    `cors_domains` gets no rights. Moonraker checks `Origin` only on the websocket, so a page on
    another site can send gcode with a form post.
  - On the unix socket, only orrery's own user and root are `admin`. Another user gets only
    what a key or an agent table gives it; Moonraker gives full rights to anyone who can
    connect. The socket is mode `0660`, and the database `0600`.
  - Host names in `trusted_clients` are skipped, with a warning: orrery doesn't look up names
    yet.
  - `orrery.toml`'s `[authorization.networks]` gives each network a role, `viewer` too, and
    replaces `trusted_clients`. The most specific network that holds an address decides its
    role. Moonraker has no roles, so it has no such rule.
  - Proxy headers count only from a trusted proxy, and from `X-Forwarded-For` only the last
    address. Moonraker reads the headers from anyone, then requires the proxy to be trusted,
    and refuses with "Proxy address not trusted"; orrery judges such a request by the
    connection's own address.
  - `trusted_proxies` is orrery's own option, and empty unless it's set. Only Moonraker's
    `trusted_clients` alone keeps Moonraker's fallback, where its networks are the proxies.
  - **A proxy on an address with a role that sends no proxy headers makes every caller look
    like the proxy,** as in Moonraker. orrery warns at startup about a trusted proxy
    that has a role. Leave a proxy on another host out of `[authorization.networks]`.
  - An agent's database namespace is the agent's own: other clients need `admin` to read or
    write it.
  - A refused request gets 403 when the client has a role, but not the one needed. The
    message says what the request would do, who may do it, the client's role and whether it
    came from its network or a key, and what that role allows: "Only an admin can reboot the
    printer's computer. This connection has the operator role, from its network, so it can
    print but can't change the printer's setup." Without any rights, it's 401
    "Unauthorized", as in Moonraker.
- **Why:** Moonraker gives every trusted device full control, which now includes rebooting
  the machine.
- **Cases:** `auth/untrusted`, `auth/proxy`, and `auth/bad_key` (both servers); `auth/keys`,
  `auth/operator`, and `auth/agent_key` (orrery only).

### partial-server-config: `server.config` leaves out components orrery doesn't have

- **Area:** config
- **Moonraker:** `server.config` reports every option of every loaded component, with defaults.
- **orrery:** it reports the options of Moonraker's default components, with Moonraker's
  defaults, in Moonraker's order, including options orrery ignores, such as `ssl_port`. Sections
  of optional components that orrery doesn't have, such as `[update_manager]` or `[power]`,
  aren't listed.
- **Why:** clients read values from this list: Fluidd reads `spoolman.server` to connect to
  Spoolman itself, and `data_store.temperature_store_size` for its charts. Matching Moonraker's
  whole list keeps clients that weren't checked working too. A listed option isn't a promise
  that orrery uses it; this file lists what orrery ignores.
- **Cases:** `server/config` passes.

### datastore: orrery keeps its own store

- **Area:** datastore import
- **Moonraker:** keeps its data in SQLite, `database/moonraker-sql.db`.
- **orrery:** keeps its data in redb, `database/orrery.redb`, and doesn't read Moonraker's file.
  Database backups are copies of the redb file. Importing Moonraker's data will be a separate
  tool.
- **Why:** see `docs/design/2026-11-01-datastore-spike.md`.
- **Cases:** none; the API behaves the same.

### file-watcher: the file watcher announces outcomes, not every step

- **Area:** files
- **Moonraker:** watches the roots with inotify, the kernel's way of reporting changes to
  folders. When a file changes on disk, it sends `notify_filelist_changed` and reads a new gcode
  file's metadata. It holds back the notification for each API change for a second, so the
  watcher's can replace it.
- **orrery:** watches the roots too, and sends the same notifications, with these differences:
  - It waits until the folders have been quiet for 0.25 s, and announces the outcome. A file
    created and removed in that time, such as a lock file, isn't announced. A file written and
    then renamed into place is one `create_file`, at its new name. Moonraker announces each
    step: `create_file` for the temporary name, then `move_file`.
  - It doesn't ask the kernel for each write, only for "closed after writing", so it doesn't
    wake for every line Klipper adds to its log. A file is announced once it's closed, as in
    Moonraker. A file still open, such as a log, shows up when a client lists the folder again.
  - API changes are announced at once, not a second later.
  - The thumbnails orrery writes into `.thumbs` aren't announced. Moonraker announces them, and
    Fluidd hides that folder.
  - If the kernel's queue of changes overflows, orrery watches every folder again and sends
    `root_update` for each root. Moonraker misses those changes.
  - `file_system_observer: none` turns the watcher off, as in Moonraker.
- **Why:** clients need to know what's in the folders, not each step that got there. Fewer
  wakeups matter on a printer's small computer.
- **Cases:** `files/watch` (both servers), `files/watch_orrery` (orrery only).

### partial-metadata: some slicers get only basic metadata

- **Area:** files
- **Moonraker:** reads metadata from PrusaSlicer and its forks, Slic3r, Cura, BambuStudio,
  Simplify3D, KISSlicer, ideaMaker, IceSL, and Kiri:Moto. It also unpacks `.ufp` uploads, runs
  gcode file processors, and can add object definitions (`enable_object_processing`).
- **orrery:** reads PrusaSlicer and its forks (SuperSlicer, OrcaSlicer, and others), Slic3r,
  Slic3r PE, BambuStudio, and Cura. A file from another slicer gets only the size, times, first
  layer height, and object height. orrery refuses `.ufp` uploads, and it has no file processors
  or object processing.
- **Why:** these are the slicers most printers use. The others can be ported one at a time.
- **Cases:** `files/metadata`, `files/metadata_plain`, `files/metascan`.

### missing-file-roots: no `config_examples` or `docs` roots

- **Area:** files
- **Moonraker:** adds the `config_examples` and `docs` roots from Klipper's folder.
- **orrery:** has the `config`, `logs`, and `gcodes` roots.
- **Why:** frontends work without them.
- **Cases:** none yet.

### log-file: orrery's log is `orrery.log`

- **Area:** files, server
- **Moonraker:** writes `logs/moonraker.log` and serves it at `/server/files/moonraker.log`.
  It starts a new file at local midnight and keeps two old ones. `server.logs.rollover` starts
  a new Moonraker or Klipper log on request.
- **orrery:** writes `logs/orrery.log`, so it doesn't mix with Moonraker's log, and serves it
  at `/server/files/moonraker.log`. It starts a new file at midnight UTC and keeps two old
  ones. `-l` sets another file and `-n` turns the file off, as in Moonraker.
  `server.logs.rollover` starts a new orrery log, and names the old one for the time, as
  Moonraker names a Klipper log it rolls over. Moonraker instead names it for the day, and
  replaces an earlier rollover from the same day. Klipper's log is rolled over as in
  Moonraker, with the differences in `no-service-control`.
- **Why:** frontends download the log by its Moonraker address.
- **Cases:** `files/log_downloads`, `server/logs_rollover`.

### history-aux-fields: history's auxiliary fields come from agents, without totals

- **Area:** history
- **Moonraker:** the `[sensor]` and `[spoolman]` components add auxiliary fields to each job,
  such as energy used or the spool, and their totals. Each field has a tracking strategy, such
  as collecting values or adding them up.
- **orrery:** has neither component. Agents set fields instead, with `orrery.agent.history_field`
  while a job is recorded, and only the fields their `[agent.<name>]` table lists in
  `history_fields`. An agent sends the field's whole value; orrery doesn't track it.
  `auxiliary_totals` is always empty.
- **Why:** an agent knows its own values, so orrery doesn't need Moonraker's trackers.
- **Cases:** `agents/history_field` (orrery only).

### spoolman-agent: Spoolman is a separate program, set up in `orrery.toml`

- **Area:** Spoolman
- **Moonraker:** a `[spoolman]` section turns on the built-in component.
- **orrery:** the `orrery-spoolman` agent does the same work, from the same section. orrery
  starts it from its own folder when `[spoolman]` has a `server`; without the program, Spoolman
  stays offline and `server.info` has a warning. Other differences:
  - The agent keeps the active spool in the `spoolman` database namespace, not in `moonraker`.
    It starts without one: `orrery-import` doesn't bring Moonraker's over.
  - A job gets its `spool_ids` field only while the agent is connected. Moonraker adds the field
    to every job.
  - When Spoolman sends no reply at all, a `proxy` request with `use_v2_response` gets
    `status_code` 500 and the error's text.
- **Why:** plugins are separate programs (docs/design/2026-11-15-plugin-interface.md).
- **Cases:** `agents/builtin_spoolman` (orrery only); none against Moonraker yet. `crates/orrery-spoolman/tests/agent.rs` checks the
  agent against stand-ins for orrery and Spoolman.

### no-python-info: `machine.system_info` has no `python` field

- **Area:** API
- **Moonraker:** `system_info.python` gives the version of the Python that runs Moonraker.
- **orrery:** leaves the field out. orrery isn't a Python program, and a made-up version would
  mislead anyone who reads it.
- **Why:** there's no true value to give.
- **Cases:** `machine/system_info`.

### no-service-control: no systemd_cli or supervisord_cli provider

- **Area:** API
- **Moonraker:** with a systemd or supervisord `provider`, `machine.system_info` lists the
  services clients can control, with their state. `machine.services.*`, `machine.reboot`, and
  `machine.shutdown` control them.
- **orrery:** with `provider: systemd_dbus`, the default, it lists the allowed services and
  follows their state over D-Bus, with `notify_service_state_changed`, and has
  `machine.services.start`, `stop`, and `restart`, and `machine.reboot` and `machine.shutdown`
  through logind. `instance_ids` come from systemd, as in Moonraker. It doesn't have the
  `systemd_cli` and `supervisord_cli` providers yet. Small differences:
  - Klipper's log rollover waits for Klipper to stop before it renames the log, refuses while
    a print is paused as well as while it prints, and starts Klipper again when the rename
    fails. Moonraker renames as soon as the stop is queued, and leaves Klipper stopped when the
    rename fails. The time in the old log's name is UTC.
  - Without logind, or with `provider: none`, reboot and shutdown fail. Moonraker falls back
    to `sudo systemctl reboot`, with the sudo password it asks clients for.
  - orrery gives one warning that lists every polkit action it isn't allowed, instead of one
    warning per action.
  - Restarting orrery's own service, and `server.restart`, start orrery's program again in the
    same process, after the usual shutdown. Moonraker restarts its service through systemd,
    and restarts itself inside Python for `server.restart`. With `provider: none`, Moonraker
    answers `ok` to a restart of its own service and does nothing; orrery restarts.
  - When polkit refuses an action, the error is 403 and says so. Moonraker passes on the D-Bus
    error.
  - orrery lists only `.service` units. Moonraker takes every unit type and cuts the name at its
    first dot, so a `klipper.socket` could stand in for `klipper.service`.
  - orrery always allows services whose names start with `orrery`, beside `moonraker` and
    `klipper` ones.
  - Agents' units are listed too, so clients can restart an agent alone. An agent's user unit
    is listed as `user/<unit>`, such as `user/orrery-spoolman`, which no Moonraker service
    name can be.
  - For orrery or Klippy started outside a service, such as from a terminal, its
    `instance_ids` entry is `(unknown)`, which can't match a real service. Moonraker gives the
    scope's name, such as `session-3`, and falls back to `moonraker` and `klipper`. A fallback
    could name another printer's Klipper on a machine with several, and an empty id makes
    Fluidd fall back the same way.
  - Moonraker saves Klipper's unit in its database, so `instance_ids.klipper` is right before
    Klippy connects. orrery finds it again on each connection.
  - For virtualization, orrery runs `systemd-detect-virt` instead of asking systemd over D-Bus.
- **Why:** service control comes in steps; see `docs/design/2026-11-19-service-control.md`.
- **Cases:** `machine/services` covers the refusals. The conformance suite runs with
  `provider: none`, so the actions were checked by hand; the D-Bus client is tested against a
  private `dbus-daemon`.

### import-gaps: what `orrery-import` doesn't bring over

- **Area:** datastore import
- **Moonraker:** keeps users, its own state, and the gcode metadata cache in `moonraker-sql.db`.
- **orrery:** `orrery-import` skips these:
  - Users' signing keys, so users log in again after the switch, with the same password.
    orrery makes them new keys.
  - Users' groups, which Moonraker stores but doesn't use. Users themselves are imported, with
    their password hashes, and Moonraker's API key is copied, as orrery's `moonraker` key. A user that orrery already has is left as it is, and so is orrery's own
    user with the same name. An LDAP user is imported, but can't log in until orrery has LDAP.
  - The `moonraker` and `database` namespaces: Moonraker's own state, such as its instance id.
    orrery keeps its own.
  - `update_manager`: Moonraker's update manager state. orrery's update agent keeps its own
    there, in its own format, and checks each entry again on its first start.
  - `gcode_metadata`: orrery reads the files again instead. It still copies each file's link to
    its last print (`job_id` and `print_start_time`) when the file hasn't changed.
  - The legacy LMDB database (`moonraker.db/`), from Moonraker before v0.9.
- **Why:** orrery has no groups yet. The rest belongs to Moonraker, or orrery rebuilds it.
- **Cases:** none; the import's unit tests read `conformance/fixtures/import/seed.db`.

### agent-null-result: an agent's `null` result is a result

- **Area:** API
- **Moonraker:** when a client calls an agent with `server.extensions.request` and the agent
  replies `"result": null`, Moonraker returns an agent error (424, "Agent <name> RPC error").
  Its check reads a missing `result` and a `null` one the same way.
- **orrery:** returns the `null` as the result. JSON-RPC allows a `null` result, and a reply is
  an error only when it has an `error` member. A reply with neither is an agent error in both.
- **Why:** Moonraker's behaviour looks accidental: its docs don't mention it, and agents such as
  aldis work around it. Agents written for Moonraker never reply `null`, so none of them change.
- **Cases:** `extensions/null_result`.

### webcam-test-no-tls: `server.webcams.test` can't reach `https` snapshots

- **Area:** API
- **Moonraker:** fetches the snapshot URL over HTTP or HTTPS to report `snapshot_reachable`.
- **orrery:** has no TLS, so an `https` snapshot URL is always reported as unreachable. Plain
  `http` URLs work as in Moonraker.
- **Why:** orrery stays free of C dependencies, and the check is only a hint for clients.
- **Cases:** none.

### update-agent: the update manager is a separate program, and has fewer entry types

- **Area:** update manager
- **Moonraker:** the built-in `update_manager` component updates `git_repo`, `web`, `zip`,
  `python`, and `executable` entries, plus implicit `klipper`, `moonraker`, and `system`
  entries, from `[update_manager]` sections in `moonraker.conf`.
- **orrery:** the `orrery-updater` agent answers Moonraker's `machine.update.*` methods and
  notifications. orrery starts it from its own folder when `orrery.toml` or, as a fallback,
  `moonraker.conf` has `[update_manager]`. What's missing or different:
  - It updates `git_repo` and `executable` entries, their Python packages and
    `system_dependencies`, and the `system` entry. A `web`, `zip`, or `python` entry is left
    out, with a warning in `server.info`.
  - An `archive` entry, orrery's own type in `orrery.toml`, installs every file
    of a release's archive and keeps a record of them in `.release-info.json`. Its status
    has `configured_type: "archive"`. See docs/design/2026-11-23-archive-entries.md.
  - **Services from entries:** the service list has each update manager entry's own service,
    from `managed_services` or by default, without a line in `moonraker.asvc`. Moonraker
    lists only `moonraker.asvc`'s. `managed_services` also takes `user/<name>`, the entry's
    unit of orrery's user, which Moonraker refuses with a warning.
  - orrery's own entry is an `archive` entry. Without the update manager's record of the
    install, `.release-info.json`, its version is unknown and it offers the newest release.
  - An `archive` or `executable` entry's unknown version shows as `v0.0.0-missing` when its
    folder isn't there, and `v0.0.0-invalid` when the folder has no record of the install
    (`release_info.json` for an executable), where Moonraker shows `?`. Clients compare
    versions as semver, so they offer an update from these but not from `?`. Git entries show
    `?`, as in Moonraker.
  - Klipper's entry is named after the project Klippy runs: `klipper` or `kalico`. Moonraker
    always calls it `klipper`. `machine.update.klipper` updates it under either name. Its
    section takes Moonraker's `channel`, `pinned_commit`, `refresh_interval`, and
    `report_anomalies`, and orrery's `enabled`. `orrery.toml` can also set `origin`,
    `primary_branch`, and `moved_origin`, which Moonraker fixes; in `moonraker.conf` they're
    ignored, with a warning. Setting both `[update_manager klipper]` and `[update_manager
    kalico]` leaves the entry out. See docs/design/2026-11-22-klipper-updates.md.
  - No `moonraker` entry. orrery's own entry, `orrery`, is an archive entry for its
    releases instead: the archive for the CPU, checked against `SHA256SUMS`, then
    `orrery preflight`, and the old set kept in `.previous/`. A development build doesn't
    update itself. Its section `[update_manager.orrery]` takes `enabled`, and `repo` from
    `orrery.toml` only. `orrery self-update` and `orrery rollback` do the same from a shell,
    with orrery or without it.
    See docs/design/2026-11-22-self-update.md.
    `machine.update.moonraker` answers 400 "orrery doesn't update Moonraker."
    `moonraker` in an entry's `managed_services` restarts nothing, since a plugin that names
    Moonraker never needs orrery restarted.
  - No `sudo apt-get` fallback. Debian packages need PackageKit, and `orrery setup` adds the
    polkit rules for it.
  - The agent keeps its state in the `update_manager` namespace of orrery's database.
    `orrery-import` doesn't bring Moonraker's over; the first check makes it again.
- **Why:** plugins are separate programs (docs/design/2026-11-15-plugin-interface.md), and the
  update manager design note (docs/design/2026-11-22-update-manager.md) decides the scope.
- **Cases:** `update/agent` and `agents/builtin_update_manager` (orrery only). The agent's unit
  tests check git, pip, GitHub releases, and PackageKit's message format against local stand-ins.

### update-safety: updates check more first, and restart less

- **Area:** update manager
- **Moonraker:** see `components/update_manager/` in Moonraker.
- **orrery:** the differences, each chosen to avoid losing work or breaking a print:
  - **Roles:** `status` needs `viewer`. `refresh` and every update, recover, and rollback need
    `admin`. Moonraker allows any authorized client.
  - **A diverged branch isn't reset** during an update. Moonraker fetches again and runs
    `git reset --hard` when the branch diverged since the last check, such as after a
    force-push, which loses local commits. orrery stops the update, and Recover fixes the repo.
  - **`full` refuses while printing,** as each single update does. Moonraker's `full` doesn't
    check.
  - **Each service restarts once,** after the last update that needs it, and not while a print
    runs: the restart waits for the print to end. Moonraker restarts after each entry.
  - **A rollback refuses a repo with changed files.** Moonraker's resets them away.
  - **A hard recover of a missing repo** clones it again from `origin`.
  - **Stable and beta channels follow the highest version tag,** not the newest by date.
  - **`moved_origin`** is matched by its URL, without a request to GitHub.
  - **An unencrypted remote,** `http://` or `git://`, is an anomaly: "Unencrypted remote
    url: …". Kalico's own repo and branch aren't anomalies.
  - **Plugin links in Klipper's folder** aren't anomalies when they point into another entry's
    repo. Klipper's entry lists them in `plugin_links`. Any other untracked source file there
    is a warning of its own, which doesn't block the update. Moonraker lists every one in its
    "Repo has untracked source files" anomaly.
  - **A plugin entry with links into Klipper's folder restarts Klipper,** whatever its
    `managed_services` say.
  - **Klipper's update stops before git** when it adds a file that's already there untracked,
    and names the file and its entry. Moonraker runs git, which refuses halfway.
  - **`preflight`,** orrery's own option, read only from `orrery.toml`: a check that the new
    version can run, before the update takes effect. For an executable, it runs the new
    program with the given arguments before the rename; a failure leaves the old program. For
    a `git_repo` entry, it runs a file in the repo after the update; a failure resets the repo
    to its old commit and restarts nothing. In `moonraker.conf`, it's ignored, with a warning.
  - **An executable keeps the program an update replaced,** in `<path>/.previous/`, so a
    rollback needs no download. Moonraker downloads the old release again.
  - **`checksums`, `programs`, and a `repo` that wins,** orrery's own options for an
    executable, read only from `orrery.toml`: a SHA-256 check of the download, an asset that's
    a `.tar.gz` of several programs, and a repository that wins over `release_info.json`.
    From `moonraker.conf`, the first two are ignored, with a warning, and `repo` is Moonraker's.
    See docs/design/2026-11-22-self-update.md, "Archives and checksums for every executable
    entry".
  - **pip isn't upgraded** before an entry's Python packages are installed.
  - **An executable's update replaces the program in one step,** so a failed download leaves
    the old one working. Moonraker deletes the old program first. Without `.previous/`, a
    rollback downloads the asset `release_info.json` names; Moonraker takes the release's
    first asset.
  - **The system check lists updates from the old package lists** when a refresh fails, and
    logs why. An update needs a working refresh, and a failed one says why. Moonraker says
    only "Error updating system packages".
  - **Periodic checks are on by default,** weekly unless `refresh_interval` says otherwise, with
    no `refresh_window` unless one is set. Moonraker checks only at startup unless
    `enable_auto_refresh` is on, and then only between midnight and 5 am.
- **Why:** docs/design/2026-11-22-update-manager.md, "Differences from Moonraker".
- **Cases:** `update/agent` (orrery only); the refusals while printing are covered there.

## Planned entries

These differences are already decided in the plan. Each gets an entry when the code that
introduces it lands:

- No TLS: `ssl_certificate_path` and `ssl_key_path` are ignored.
- `moonraker.conf` support is best effort: `[secrets]`, Jinja templating, and full option
  validation come later.
- Legacy LMDB databases from before Moonraker v0.9 can't be imported.
- Out-of-scope components (power, sensors, MQTT, and others) are missing or stubbed.

### button-templates: buttons run actions, not templates

- **Area:** config, API
- **Moonraker:** a `[button]` section runs Jinja templates on press and release, with
  `call_method`, `send_notification`, `event`, and `user_data`. Klipper's macros may call any
  of Moonraker's remote methods, including `shutdown_machine` and `reboot_machine`.
- **orrery:** a button runs lists of [actions](configuration/actions.md), on `on_press`,
  `on_release`, `on_click`, and `on_long_press`, through the `orrery-gpio` agent. What's
  different:
  - From `moonraker.conf`, only a template that's a single `call_method` with fixed arguments
    converts. Anything else is left out, with a warning that names the button and the option.
  - `notify_button_event` is sent for every event that runs actions, with `{name, event}`.
    Moonraker sends it only from `send_notification`, with `{name, type, event, aux}`.
  - Klipper's remote methods run actions as `operator`. `shutdown_machine` and
    `reboot_machine` run as `admin` with `[moonraker_compat] klipper_shutdown`, on by default.
  - The remote method `run_action` is orrery's own.
- **Why:** a Jinja engine is a large dependency, and a conversion that guesses wrong runs the
  wrong thing when someone presses the button. See docs/design/2026-11-23-buttons-and-actions.md.
- **Cases:** none.
