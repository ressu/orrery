# Klipper's own updates

**Proposal (2026-11-22):** the update agent gets an entry for Klipper, as Moonraker has. It
detects whether the printer runs Klipper or Kalico, names the entry after it, `klipper` or
`kalico`, and expects that project's own repo. A repo with another remote is an anomaly, so
the user learns that their setup changed, and the update still runs. The entry also reports
which plugins changed Klipper's own folder.

**Status (2026-11-22):** approved. Steps 2 to 4 are built: the entry, Klipper or Kalico with
its name, the remotes, and the plugin links. Step 5 isn't.

"Klipper" here means Klipper or Kalico, Klipper's fork. Both printers run Kalico, and both
work the same way for updates.

## What the printers have

Both bluev2 and pink-0 run Kalico from `~/klipper`, on the `main` branch, tracking
`https://github.com/KalicoCrew/kalico`. Klippy runs from `~/klippy-env`. `printer.info` gives
both paths, and names the program:

```json
{"app": "Kalico", "klipper_path": "/home/printer/klipper",
 "python_path": "/home/printer/klippy-env/bin/python",
 "software_version": "v2026.11.00-30-g991e1823"}
```

**Plugins are linked into Klipper's folder.** Most plugins install a link in
`klippy/extras` that points to a file in the plugin's own repo. bluev2 has six:

| Link in `~/klipper/klippy/extras` | Points to |
|---|---|
| `auto_speed.py` | `~/klipper_auto_speed/auto_speed.py` |
| `auto_tap.py` | `~/klipper_auto_tap/auto_tap.py` |
| `autotune_tmc.py`, `motor_constants.py`, `motor_database.cfg` | `~/klipper_tmc_autotune/` |
| `led_effect.py` | `~/klipper-led_effect/src/led_effect.py` |

bluev2 also has `filter_monitor.py` in `klippy/plugins`, Kalico's own folder for plugins. Git
ignores that folder, so a plugin there doesn't leave the repo with untracked files. Klipper
has no such folder.

**The firmware matches the host.** On bluev2, `mcu`, `mcu host`, and `mcu toolhead` all
report `v2026.11.00-30-g991e1823`, the same as Klippy. Keeping it that way is aldis's work, not
the update agent's (user, 2026-11-22).

## What Moonraker does

- **The entry is always there.** Moonraker builds `klipper` from Klippy's `klipper_path` and
  `python_path` once Klippy connects (`update_manager.py:227-247`). Its defaults
  (`common.py:35-42`):
  - `origin`: `https://github.com/Klipper3d/klipper.git`
  - `moved_origin`: `https://github.com/kevinoconnor/klipper.git`
  - `requirements`: `scripts/klippy-requirements.txt`
  - `install_script`: `scripts/install-octopi.sh`, read for its package list
  - `managed_services`: `klipper`
- **Only a few options can change:** `channel`, `pinned_commit`, `refresh_interval`, and
  `report_anomalies` (`OPTION_OVERRIDES`, `common.py:44`). `origin` and the primary branch
  can't.
- **Any other remote is only an anomaly.** For Kalico, Moonraker reports "Unofficial remote url"
  and "Repo not on official remote/branch, expected: origin/master, detected: origin/main"
  (`git_deploy.py:729-743`). The Update button still works, and updates from whatever the repo's
  remote is.
- **Plugin links are anomalies too:** "Repo has untracked source files", listing every link.
  Moonraker can't tell a plugin it updates from a file nobody tracks.
- **Kalico's users asked for this to change.** Moonraker issue
  [#977](https://github.com/Arksine/moonraker/issues/977), "Add Kalico support" (March 2025,
  still open), asks Moonraker to recognize Kalico's repo and branches. Arksine answered that
  "updates are allowed to proceed" from forks, but "Moonraker only guarantees support for the
  official repo", and that he'd "like to avoid adding any specific code" for forks. He added
  `report_anomalies`, per repo, which hides the anomalies from frontends.

## The risks

1. **Where the code comes from.** Klippy controls the heaters and motors, so this is a safety
   issue. An update installs whatever the repo's remote serves. An unofficial mirror can stop
   following its project, and then the printer never gets the project's safety fixes, while
   every update reports success. Moonraker's only sign is an anomaly, and for Kalico it's the
   same anomaly every Kalico user learns to ignore.
2. **Plugins change Klipper's code.** Updating a plugin repo changes a file that Klippy
   loads. That's the same question as for Klipper's own remote, asked for every plugin.
3. **Files nobody updates.** A file in `klippy/extras` that's neither Klipper's nor a linked
   file from a known plugin entry is code that no update covers. It may be a plugin someone
   copied in by hand.
4. **An update that collides with a plugin.** If Klipper adds a file with a plugin's name,
   such as `led_effect.py`, git refuses the update because of the untracked link.

Neither Klipper nor Kalico signs its tags, and most commits are unsigned, so signatures
can't answer the first risk. The remote's address has to: an anomaly is useful only when the
right setups don't get one.

## Proposal

### The entry

- **orrery adds the entry by itself**, as Moonraker does, from `printer.info`'s `klipper_path`
  and `python_path`. Fluidd expects it. `enabled = false` in the entry's section of
  `orrery.toml` leaves it out.
- **Python packages and system packages** work as for any `git_repo` entry:
  `scripts/klippy-requirements.txt` into Klippy's virtualenv, and the package list in
  `scripts/install-octopi.sh`. Both Klipper and Kalico have these files.
- **The entry restarts `klipper`** once, after the last update that needs it, and not while a
  print runs. That's how every entry works already.

### Klipper or Kalico

- **The agent detects which one runs,** from `printer.info`'s `app` and the repo's remote, and
  uses that project's repo, primary branch, and old address:

  | Project | Remote | Primary branch | Moved from |
  |---|---|---|---|
  | Klipper | `https://github.com/Klipper3d/klipper` | `master` | `https://github.com/kevinoconnor/klipper` |
  | Kalico | `https://github.com/KalicoCrew/kalico` | `main` | `https://github.com/DangerKlippers/danger-klipper` |

  The remote matches with or without `.git`. Kalico on `main` then gets no anomalies, which is
  what issue #977 asks Moonraker for.
- **The entry is named after the project:** `klipper` or `kalico`. Moonraker always calls it
  `klipper`. Naming it `kalico` makes Kalico a first-party project in orrery's update list,
  not a fork that Moonraker reports as unofficial (user, 2026-11-22).
- **`machine.update.klipper`** updates that entry, whichever its name, so a client that calls
  it keeps working.
- **Fluidd handles a `kalico` entry.** Its Update button calls `machine.update.klipper` for an
  entry named `klipper`, and `machine.update.client` with the name for any other entry
  (`VersionSettings.vue`, `handleUpdateComponent`). orrery's `client` updates any entry, so
  both work. Recover and rollback send the entry's name. Mainsail, KlipperScreen, and
  Mobileraker aren't checked yet.
- **Its options** come from `[update_manager.kalico]` or `[update_manager.klipper]` in
  `orrery.toml`, or `[update_manager klipper]` in `moonraker.conf`, which Kalico's own docs
  tell users to write. Either name works for either project. Both sections at once is an
  error, since neither is clearly the one meant: orrery leaves the entry out, and the warning
  says to keep only one (user, 2026-11-22).

### Remotes

Every `git_repo` entry already works this way, as in Moonraker. Klipper's entry follows
the same rule, with the detected project as its `origin`.

- **A remote the entry doesn't expect is an anomaly,** as in Moonraker: "Unofficial remote url:
  https://github.com/someone/klipper". For Klipper's entry, the expected remote is the detected
  project's. For another entry, it's the entry's `origin`. The anomaly tells the user to check
  their setup when something changed, such as a plugin that moved or a mirror they forgot.
- **The update still runs,** as in Moonraker. A strict mode, which refuses updates with
  anomalies, may come later (user, 2026-11-22).
- **`report_anomalies = false`** hides the entry's anomalies from frontends, as in Moonraker.
  The agent still logs them once, when they first appear.
- **For Klipper's entry, `orrery.toml` can name the expected source:** `origin`, `primary_branch`,
  and `moved_origin`. Moonraker fixes those three. A printer that runs a fork on purpose names
  it there, and then gets no anomaly:

  ```toml
  [update_manager.klipper]
  origin = "https://github.com/someone/klipper"
  primary_branch = "main"
  ```

  Only `orrery.toml` can change them. In `moonraker.conf` they're ignored, with a warning, as
  Moonraker ignores them.
- **An unencrypted remote** (`http://`, `git://`) is an anomaly too, for every `git_repo`
  entry: "Unencrypted remote url: http://…". HTTPS and SSH remotes are encrypted.
- **How the agent knows the file:** `orrery.agent.config` names the options that came from
  `orrery.toml`, in `from_orrery_toml`.

### Plugins in Klipper's folder

- **The agent finds the links.** Each untracked file in Klipper's repo that links into another
  `git_repo` entry's folder belongs to that entry. Klipper's entry lists them in
  `plugin_links`, by entry, and they aren't anomalies. An entry whose folder holds Klipper's
  own, such as one for the home folder, isn't a plugin. The links are found when Klipper's
  entry is set up and at each refresh; that's a local `git ls-files`, with no network.
- **Other untracked files are warnings**, named one by one: "klippy/extras/foo.py isn't
  Klipper's, and no update entry has it." The update still runs (user, 2026-11-22).
- **A plugin entry that links into Klipper restarts `klipper`**, whatever `managed_services`
  says, since its update changes what Klippy loads.
- **A collision stops the update** before git does, with both names: "Kalico's update adds
  klippy/extras/led_effect.py, which is the led_effect entry's link. Remove the link, then
  update again." For a file no entry has: "… which is already there and isn't Kalico's. Move
  it away, then update again."
- **Kalico's `klippy/plugins` folder** is checked the same way. Git ignores it, so its files
  are never warnings, and they can't collide with an update. That's what Kalico made it for.

### Firmware

The update agent doesn't check or report the MCUs' firmware (user, 2026-11-22). A message
about it isn't something a user can act on from the update page. aldis already fills that gap,
with more options than the update agent could offer.

### Channels

As for any `git_repo` entry, `dev` follows the primary branch, and `stable` and `beta` follow
the highest version tag. Klipper's tags are `v0.13.0`, and Kalico's are monthly, such as
`v2026.11.00`. Both sort correctly by version. The default is `dev`, as in Moonraker.

## Differences from Moonraker

1. Klipper or Kalico is detected, and its own repo and branch aren't anomalies.
2. The entry is named `kalico` for Kalico. Moonraker always calls it `klipper`.
3. `orrery.toml` can set the `origin`, `primary_branch`, and `moved_origin` of Klipper's entry.
   Moonraker fixes them.
4. An unencrypted remote, `http://` or `git://`, is an anomaly.
5. Plugin links that belong to an update entry aren't anomalies. Other untracked files are
   warnings, one by one.
6. A plugin entry that links into Klipper's folder always restarts Klipper.
7. An update that collides with a plugin link stops before git does, and says why.
8. `enabled = false` in the entry's section leaves the entry out.

## The order of work

1. This note, reviewed.
2. The entry: paths from `printer.info`, Klipper or Kalico, and its name. A check that the
   unit orrery restarts runs that Klippy was built and dropped as more than the entry needs
   (user, 2026-11-22).
3. Remotes: the detected project as the entry's `origin`, the anomaly for a remote
   without HTTPS, and `origin`, `primary_branch`, and `moved_origin` in `orrery.toml`.
4. Plugin links: finding them, the warnings, the restarts, and the collision check.
5. Docs and tests, then an update on pink-0, whose Kalico is behind its remote.
