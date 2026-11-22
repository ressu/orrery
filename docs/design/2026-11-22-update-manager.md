# Update manager

**Decision (2026-11-22):** orrery gets an update manager as a built-in agent, `orrery-updater`,
started as `orrery-spoolman` is. Moonraker's `machine.update.*` names map to it, so Fluidd's
update page works without changes. Its config lives in `orrery.toml`, as all of orrery's does
(user, 2026-11-22). The `[update_manager]` sections in `moonraker.conf` are only a fallback
that eases the move, so a migrated printer works before its config is converted.

**Status (2026-11-22):** built, as `crates/orrery-updater`. The differences from Moonraker are
`update-agent` and `update-safety` in `docs/regressions.md`.

The update manager was one of the last gaps that kept orrery from daily use on bluev2. Before
it, orrery had no `machine.update.*` methods, so Fluidd showed no update list.

## What bluev2 has

`moonraker.conf` sets `[update_manager] refresh_interval: 168` and has ten entries:

| Entry | Type | What the update needs |
|---|---|---|
| Klipper-Adaptive-Meshing-Purging | `git_repo`, `channel: dev` | Restart Klipper |
| mobileraker | `git_repo` | `requirements` into `mobileraker-env`, restart `mobileraker` |
| klipper_tmc_autotune | `git_repo`, `channel: dev` | Restart Klipper |
| klipper_auto_speed | `git_repo` | Restart Klipper |
| Klippain-ShakeTune | `git_repo` | `requirements` into its virtualenv, `system_dependencies`, restart Klipper |
| led_effect | `git_repo`, `is_system_service: False` | Nothing |
| crowsnest | `git_repo` | Restart `crowsnest` |
| filter_monitor | `git_repo` | Restart Klipper (and `moonraker`, see below) |
| printerkeep | `executable`, `repo: ressu/printerkeep` | Nothing; it runs from a timer |

Moonraker adds three entries by itself: `klipper`, `moonraker`, and `system` (the printer's
Debian packages). All the repos are clean except Klipper, which has seven untracked files:
plugins link their modules into `klippy/extras/`.

## What's in the first build

| Type | First build | Notes |
|---|---|---|
| `git_repo` | Yes | With `requirements` and `system_dependencies` |
| `executable` | Yes | A binary from GitHub releases |
| `system` | Yes | Debian packages through PackageKit |
| `web`, `zip` | Backlog | Frontends such as Fluidd; built when someone needs it. The user's frontends are installed elsewhere |
| `python` | Later | A package from PyPI; a `server.info` warning until then |
| `klipper` (implicit) | No | See [Klipper's own updates](#klippers-own-updates) |
| `moonraker` (implicit) | No | orrery replaced it |

An entry with an unknown type, or one that isn't built yet, gets one `server.info` warning that
names it, and the other entries keep working.

## Config

`orrery.toml` is where the update manager is set up. The section and option names are
Moonraker's, as for the rest of orrery's config: `[update_manager]` holds the global options,
and `[update_manager.<name>]` is one entry, the `[update_manager <name>]` of `moonraker.conf`.

```toml
[update_manager]
refresh_interval = 168

[update_manager.crowsnest]
type = "git_repo"
path = "~/crowsnest"
origin = "https://github.com/mainsail-crew/crowsnest.git"
managed_services = ["crowsnest"]

[update_manager.printerkeep]
type = "executable"
channel = "stable"
repo = "ressu/printerkeep"
path = "~/printerkeep"
is_system_service = false
```

Values have TOML's types: lists are lists, and booleans are `true` and `false`.

**`moonraker.conf` is a fallback.** orrery reads its `[update_manager]` sections too, as it
reads the rest of that file, and `orrery.toml` wins wherever both set an option. This matters
after the move as well: plugin installers, such as crowsnest's, add their entries to
`moonraker.conf`, and orrery picks them up without an edit. The config-upgrade note's warning
for options whose values differ between the two files covers these sections too.

**`orrery config upgrade`** copies the sections into `orrery.toml`, like the other sections
orrery reads. This changes the config-upgrade note, which lists `[update_manager]` among the
sections that aren't copied.

**Later, installers get a tool** that adds their entry to `orrery.toml` (see "A config tool for
installers" in `2026-11-22-config-upgrade.md`). Until then, they keep writing to
`moonraker.conf`, and the fallback reads it.

**The options:**

| Where | Options |
|---|---|
| `[update_manager]` | `refresh_interval` (hours, default 168), `enable_auto_refresh`, `refresh_window`, `channel`, `enable_system_updates` |
| Every entry | `type`, `path`, `channel`, `refresh_interval`, `managed_services`, `is_system_service`, `info_tags`, `report_anomalies` |
| `git_repo` | `origin`, `moved_origin`, `primary_branch`, `pinned_commit`, `virtualenv`, `requirements`, `system_dependencies`, `install_script` |
| `executable` | `repo`, `asset_name` |

## The API

The methods are Moonraker's (`update_manager.py:151-182`). Each one has the HTTP form
`/machine/update/<name>` too.

| Method | Parameters | What it does |
|---|---|---|
| `machine.update.status` | `refresh` | Returns the state of every entry |
| `machine.update.refresh` | `name` | Checks one entry, or all of them, for updates |
| `machine.update.upgrade` | `name` | Updates one entry, or all of them |
| `machine.update.client` | `name` | Updates one entry; without `name`, the same as `full` |
| `machine.update.full` | none | Updates every entry: `system` first, then the rest |
| `machine.update.system` | none | Updates the Debian packages |
| `machine.update.recover` | `name`, `hard` | Repairs a git repo |
| `machine.update.rollback` | `name` | Goes back to the version before the last update |
| `machine.update.klipper` | none | Refused: "Klipper updates aren't available in orrery yet." |
| `machine.update.moonraker` | none | Refused: "orrery doesn't update Moonraker." |

Fluidd calls `status`, `refresh`, `recover`, `client`, `system`, `full`, `klipper`, and
`moonraker` (`src/api/socketActions.ts:71-169`). It never calls `upgrade` or `rollback`.

**Notifications:**

- `notify_update_response` reports progress: `{message, application, proc_id, complete}`.
  Fluidd shows the messages in its update dialog, and closes the dialog on `complete: true`.
  During `full`, `complete` is true only at the very end.
- `notify_update_refreshed` carries the same data as `status`, after a refresh or an update.

**What `status` returns:** `{version_info, busy, github_rate_limit, github_requests_remaining,
github_limit_reset_time}`. `version_info` holds one object per entry. These are the fields Fluidd
reads (`src/store/version/getters.ts`, `src/components/settings/VersionSettings.vue`):

| Entry type | Fields |
|---|---|
| Every type | `name`, `configured_type`, `version`, `remote_version`, `is_valid`, `channel`, `warnings`, `anomalies`, `info_tags` |
| `git_repo` | `is_dirty`, `current_hash`, `remote_hash`, `commits_behind` (`sha`, `author`, `date`, `subject`, `message`, `tag`), `owner`, `repo_name`, `branch`, `remote_url`, `full_version_string` |
| `executable` | `owner`, `repo_name`, `rollback_version`, `last_error` |
| `system` | `package_count`, `package_list` |

orrery fills Moonraker's other fields too, such as `detached`, `pristine`, `corrupt`, and
`commits_behind_count`, when they cost nothing extra. Fluidd uses `is_dirty` and `is_valid` to
pick its button: a dirty repo gets a soft Recover, and an invalid one a hard Recover.

## How each type works

The agent runs `git` and `pip` as programs, as Moonraker does. A git library would add
megabytes to the agent. GitHub requests use `reqwest` with rustls, which the Spoolman agent
already uses.

### `git_repo`

- **Refresh:** `git fetch --prune` from the configured remote, then `git status --porcelain -b`,
  `git describe`, and `git log HEAD..<remote branch>` for the commits behind. Channel `dev`
  follows the branch. Channels `stable` and `beta` follow its highest release tag, or its
  highest tag that isn't an alpha for `beta`. Moonraker takes the newest tag by date instead,
  and two tags made in the same second then come in either order.
- **Validity,** as in Moonraker (`git_deploy.py:1087-1106`): the repo is valid when its branch
  and the remote it tracks are known, and it's not dirty, detached, or diverged. Then
  `warnings` says why. A remote URL other than `origin`, or a branch other than
  `primary_branch`, is only an anomaly.
- **`moved_origin`:** when the remote's URL is `moved_origin`, orrery points the remote at
  `origin`. Moonraker instead requests the old URL and follows GitHub's redirect to find out.
- **Dirty:** a repo with changed tracked files is dirty, and an update refuses it. Untracked
  files don't make it dirty, as in Moonraker (`git describe --dirty`, `git_deploy.py:368`), so
  Klipper's plugin links don't block updates. They make `pristine` false.
- **Update:** a fetch and a fresh check, then a fast-forward to the commit the check found.
  An invalid repo is refused, with the reason. orrery stores the commit it left as the
  rollback point. Then the dependencies the update added are installed, as in Moonraker
  (`app_deploy.py:436-470`):
  - **Python packages:** when `requirements` lists a line it didn't list before,
    `<virtualenv>/bin/python -m pip install -U -r <requirements>`, with
    `pip_environment_variables` set. It's tried three times. `requirements` counts only for
    an entry with a virtualenv.
  - **System packages:** when `system_dependencies` lists a package it didn't list before,
    PackageKit refreshes its package lists, then installs the listed packages that aren't
    installed yet, from signed sources only. The file lists packages per Linux release, with
    conditions such as `distro_version >= '24.10'`, read as Moonraker reads them.
  - A failed install is reported in the update dialog, and the update goes on, as in
    Moonraker.
  - Then the services in `managed_services` restart (see [Restarts](#restarts)).
  - A recover with `update_deps` installs everything the files list. A rollback installs
    what the older version added.
  - The output of pip and PackageKit goes to the update dialog and to the debug log only, so
    an update writes a few log lines, not hundreds. A package source that PackageKit names
    more than once is shown once.
  - **pip itself isn't upgraded.** Moonraker upgrades an old pip in the virtualenv first.
- **`install_script`:** orrery reads it for its `PKGLIST="…"` lines, as Moonraker does, and
  never runs it. Moonraker treats it as a deprecated way to list packages.
- **Recover, soft:** `git checkout <primary_branch>` and `git reset --hard <remote>/<branch>`.
  Tracked changes are lost; untracked files stay.
- **Recover, hard:** a fresh clone into a folder beside the repo, which then replaces the repo,
  as in Moonraker (`git_deploy.py:926-956`). A failed clone leaves the repo as it was. The old
  repo is moved aside first and removed only once the clone is in its place. A missing repo
  is cloned from `origin`; Moonraker can't recover it, because it reads the URL from the repo.
  Untracked files are lost, such as Klipper's plugin links, so plugins need their install
  scripts run again.
- **Rollback:** `git reset --hard` to the stored rollback commit, then the same dependency and
  restart steps as an update. A repo with changed files is refused, and a recover resets it.
  Moonraker's rollback resets it, and the changes are lost.

### `executable`

- **Refresh:** GitHub's `releases/latest` for `stable`, or the newest release for `beta`, as in
  Moonraker. The release's tag is the `remote_version`. `release_info.json` in `path` gives
  the installed version and the asset's name; its `project_owner/project_name` wins over
  `repo`, with an anomaly, as in Moonraker. An entry without a valid `release_info.json`,
  outside a writable folder, or inside a git repo is invalid, and isn't checked.
- **Channels:** `stable` and `beta` only. `channel: dev` falls back to `stable` with Moonraker's
  warning.
- **Update:** the release's asset that `release_info.json` names is downloaded to a hidden file
  beside the program, made executable for its owner and group, and renamed over the old
  program in one step. Then `release_info.json` gets the new version, written the same way
  and with the old file's permissions. A failed or short download leaves the old program as
  it was. Moonraker deletes the old program before it moves the new one in.
- **Rollback:** the release the last update replaced. The update keeps the old program and
  its `release_info.json` in `<path>/.previous/`, so a rollback renames them back without the
  network (docs/design/2026-11-22-self-update.md, 2026-11-22). Without `.previous/`, the
  release is downloaded again by its tag. orrery downloads the asset `release_info.json`
  names; Moonraker takes the release's first asset, which is the wrong program when a release
  has one per architecture.
- **Recover** is refused, as in Moonraker: "Updater printerkeep is not a Git Repo Type".
- **`system_dependencies`** works as for `git_repo`. An executable has no Python packages.
- **Rate limits:** GitHub allows 60 requests an hour without a token. `status` reports what's
  left, as Moonraker does. A check sends the `ETag` from the last answer, so an unchanged
  release answers with 304 and no body. GitHub still counts the request without a token.
- **HTTPS** uses `reqwest` with rustls and the system's certificates, as the Spoolman agent
  does.

### `system`

PackageKit is the system's package service. orrery talks to it over D-Bus with the client it
already has for systemd (`crates/orrery-dbus`).

- **The entry** is there unless `enable_system_updates: False`, as in Moonraker. It comes first
  in `version_info`, and `full` updates it first. Its object is Moonraker's: `name`,
  `configured_type`, `package_count`, and `package_list`.
- **Refresh:** `RefreshCache`, then `GetUpdates`. `package_list` holds the package names. When
  the refresh fails, such as for one source with a bad signature, the updates still come from
  the lists PackageKit has, and the error is logged. Moonraker lists nothing new then, and
  keeps showing its last list.
- **Update:** a fresh `RefreshCache` and `GetUpdates`, then `UpdatePackages` with
  `ONLY_TRUSTED`, so packages from unsigned sources aren't installed. A failed refresh stops
  the update. PackageKit's package and source signals become `notify_update_response`
  messages, as in Moonraker; its percentages don't, yet. An error says why, where Moonraker
  says only "Error updating system packages". Nothing restarts after it, as in Moonraker.
- **Rollback** is refused: "Rollback not available for system". So is Recover.
- **No `sudo apt-get` fallback.** Moonraker falls back to it without PackageKit. orrery reports
  that PackageKit isn't there instead. This is a regression, built only if someone asks.

## Klipper's own updates

Moonraker adds a `klipper` entry from Klippy's own paths: the repo Klippy runs from, its
virtualenv, `scripts/klippy-requirements.txt`, and `scripts/system-dependencies.json`. orrery
doesn't copy that (user, 2026-11-22): Moonraker updates Klipper from whatever remote the repo
has. The safer design is in [2026-11-22-klipper-updates.md](2026-11-22-klipper-updates.md).
Until it's built, `machine.update.klipper` is refused, and Klipper is updated by hand or with
KIAUH.

## Restarts

- **One restart per service.** When several entries name the same service, as four of bluev2's
  entries name Klipper, the service restarts once, after the last of them. This holds for
  `full` and for updates that queue. Moonraker restarts Klipper after each one, except in
  `full`.
- **The agent asks orrery** with `machine.services.restart`, and orrery checks its
  allowed-services list as for any other caller. The agent never talks to systemd itself.
- **`moonraker` in `managed_services` restarts nothing** (user, 2026-11-22). A plugin that names
  Moonraker depends on Moonraker itself, never on orrery, so no update needs orrery restarted.
  orrery skips the name with one log line. filter_monitor's `klipper moonraker` restarts only
  Klipper.
- **`is_system_service: False`** means the entry's own service isn't restarted by default, as
  for led_effect and printerkeep. Without `managed_services`, an entry restarts only its own
  service.
- **A print that starts during an update** holds the restarts back until it ends. Moonraker
  restarts the services anyway.

## Printing

Every update, recover, and rollback refuses while a print is running or paused, and so does a
refresh. The agent follows `print_stats.state` with a subscription. `full` refuses too, which
Moonraker's `full` doesn't check (`update_manager.py:329`). One command runs at a time, and
`busy` is true while it runs.

## Refreshing

The agent checks for updates by itself, and never installs them by itself.

- **How often:** `refresh_interval`, in hours, as in Moonraker. The default is 168, a weekly
  check. Daily checks or rarer ones are enough for most printers (user, 2026-11-22). Someone
  who wants the newest of everything can set 24, and someone who wants fewer checks can set
  720 for about a month. Each entry can set its own `refresh_interval`.
- **Later, durations in words** (user, 2026-11-22; low priority). TOML has dates and times, but
  no type for a length of time, so `refresh_interval` is a number of hours. orrery could also
  take a string, such as `"1d"`, `"1w"`, or `"daily"`, while a number keeps meaning hours, as in
  Moonraker. It would suit orrery's other intervals too.
- **When:** the agent sleeps until the next entry is due. If a print is running then, it
  waits for the print to end. It doesn't wake up on a timer between checks.
- **At startup,** the agent checks only the entries that are due, or whose config changed.
  So a restart doesn't fetch ten repos.
- **Periodic checks are on by default** (user, 2026-11-22). `enable_auto_refresh = false` turns
  them off, and then the agent checks only at startup.
- **How Moonraker does it:** `enable_auto_refresh` is off by default, and then Moonraker checks
  only at startup (`update_manager.py:202-207`). A Moonraker that runs for weeks never checks
  again. With it on, Moonraker wakes every hour and checks the entries that are due
  (`base_deploy.py:61-64`), only between midnight and 5 am.
- **`refresh_window`** limits the checks to some hours of the day, such as `0-5`. orrery has no
  window by default, because a printer that's off at night would never check. A check that
  falls outside the window waits for it to open.
- **No automatic installs** (user, 2026-11-22). Many plugins and tools aren't strict about
  backwards compatibility, so an update nobody asked for can break a printer. Installing
  updates without being asked is out of scope, and would need its own design.
- **A refresh by hand** is `machine.update.refresh` or `status` with `refresh: true`. Moonraker
  skips a refresh within 60 seconds of the last one, and so does orrery.

## State

The agent keeps each entry's state in orrery's database, in the namespace `update_manager`: the
last refresh time, a hash of the entry's config, the rollback commit or version, and the last
refresh's results. It writes only after a refresh or an update, to spare the SD card.
`orrery-import` already skips Moonraker's copy of this data (see `docs/regressions.md`), so the
first startup after the switch refreshes everything once.

## Roles

Moonraker lets every trusted client do everything. orrery splits the methods by role, as
`2026-11-20-authorization.md` splits the rest:

| Methods | Role |
|---|---|
| `status` | `viewer` |
| `refresh`, `upgrade`, `client`, `full`, `system`, `recover`, `rollback` | `admin` |

An update installs packages and restarts services, which are admin rights already. A refresh
is admin too (user, 2026-11-22): only someone who can act on the result needs it. That may
change if some entries later get roles of their own, so that an operator could update them.

The built-in agent gets `call_role = "admin"`, and `call_roles` gives `status` to viewers (see
"Agents" in the authorization note).

**The agent itself** has the `admin` role, because it calls `machine.services.restart`.
Processes of the same user are admin anyway (see the authorization note).

## Permissions

PackageKit checks polkit as systemd does. The agent runs as a child of `orrery.service`, so it
has the `orrery-admin` group. Both rules that `orrery setup` writes, the normal and the strict
one, always get three PackageKit actions (user, 2026-11-22):

- `org.freedesktop.packagekit.system-sources-refresh`
- `org.freedesktop.packagekit.system-update`
- `org.freedesktop.packagekit.package-install`

This reverses the service-control note, which left PackageKit out because orrery didn't update
packages. `package-install` lets orrery install any package from a trusted source, which is
root in all but name. Moonraker's rule allows it too.

**Later:** the strict rule could get the PackageKit actions only when the update manager is
on. It's a small change, left for when someone wants it.

## Where it runs

`orrery-updater` is a built-in agent, like Spoolman's. It was first called `orrery-update`,
which read like orrery's own self-update (renamed 2026-11-22).

- orrery turns it on when `orrery.toml` has `[update_manager]`, or, as the fallback,
  `moonraker.conf` has it. orrery starts it from its own folder with `command`.
- The agent's name is `update_manager`, so `[update_manager]` and `[update_manager.<name>]`
  are its own sections, which it may read, as `[spoolman]` is the Spoolman agent's.
- It maps `machine.update` to `agent.update_manager`, and the events `update_response` and
  `update_refreshed` to Moonraker's notifications. `server.info` lists the component
  `update_manager`.
- An `[agent.update_manager]` table in `orrery.toml` overrides the built-in options one at a time.
- The agent reads its config with `orrery.agent.config`, the way the Spoolman agent does.
- For PackageKit, the agent uses orrery's D-Bus client, which moved from
  `crates/orrery/src/dbus/` into its own crate, `crates/orrery-dbus`, so both can use it.

## Differences from Moonraker

Each one is in `docs/regressions.md`, under `update-agent` or `update-safety`.

1. Methods have roles: `status` for viewers, the rest for admins.
2. A branch that diverges during an update isn't reset. A diverged repo is invalid in Moonraker
   too, but Moonraker fetches again as it updates, and runs `git reset --hard` if the branch
   diverged since the last check, such as after a force-push (`git_deploy.py:169-174`). That
   loses local commits. orrery stops the update instead, and the user picks Recover.
3. Stable and beta follow the highest tag, not the newest by date.
4. `moved_origin` is matched by URL, without a request to GitHub.
5. `full` refuses while printing.
6. Each service restarts once per batch of updates, and not while a print runs.
7. A rollback refuses a repo with changed files.
8. A hard recover of a missing repo clones it from `origin`.
9. pip isn't upgraded before the Python packages are installed.
10. An executable's update replaces the program in one step, and its rollback downloads the
    asset `release_info.json` names, not the release's first one.
11. The system check lists updates from the old package lists when a refresh fails, and a
    failed system update says why.
12. No `klipper` or `moonraker` entry, and `moonraker` in `managed_services` restarts nothing.
13. No `sudo apt-get` fallback.
14. `web` and `zip` entries are in the backlog, and `python` entries come later.
15. Periodic checks are on by default, weekly unless `refresh_interval` says otherwise, with no
    `refresh_window` unless one is set.

## The order of work

1. This note, reviewed.
2. orrery: roles per mapped method, the built-in `update_manager` agent, the shared D-Bus crate, and
   PackageKit in `orrery setup`'s rules.
3. `orrery-updater`: config, state, `status` and `refresh`, the command lock, the print check,
   and progress notifications.
4. `git_repo`: update, recover, rollback, and restarts.
5. `requirements` and `system_dependencies`.
6. `executable`.
7. `system`.
8. `docs/regressions.md`, `conformance/coverage.md`, `just copy-to-printer`, and the README.

**Testing:** unit tests on temporary git repos with local bare remotes, for a repo that's
behind, dirty, untracked, diverged, detached, or tagged. A local server stands in for GitHub
releases. Conformance cases against Moonraker cover `status` and the refusals while printing,
where Moonraker accepts a local remote. On bluev2, Fluidd's update list is compared with what
Moonraker showed, and the idle agent's memory is measured.

## Open questions

1. **Klipper's own updates:** what a safe design looks like (see above).
