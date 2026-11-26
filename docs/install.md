# Installing Orrery

This page covers the installer, installing by hand, importing Moonraker's data, updating, and
going back to Moonraker. For the short version, see the [README](../README.md#installing).

## The installer

Run this on the printer, as the user Moonraker runs as:

```
curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh -s -- --import --migrate
```

The installer puts Orrery in `~/orrery`, imports Moonraker's data, and switches the printer
from Moonraker to Orrery. Fluidd, Mainsail, and KlipperScreen then connect to Orrery as they
did to Moonraker.

- It checks the download against the release's checksums.
- It stops if the printer is printing.
- It asks for your password for `sudo` when it stops Moonraker and sets up Orrery's service.
- If Orrery is installed already, it stops. To update Orrery, see
  [Updating Orrery](#updating-orrery).

Without `--import` and `--migrate`, the installer asks about each one. Its other options:

| Option | What it does |
|---|---|
| `--no-import`, `--no-migrate` | Skip the import, or the switch to Orrery, without asking |
| `--no-alias-moonraker` | Keep Moonraker's service file, as when you try Orrery next to Moonraker |
| `--version <vX.Y.Z>` | Install that release, not the latest |
| `-d <folder>` | The printer's data folder, if it isn't `~/printer_data` |
| `--no-setup` | Install the programs only, without Orrery's service |

### `moonraker.service` becomes Orrery

Some services start Moonraker, or wait for it, such as aldis. So that they get Orrery instead,
setup makes `moonraker.service` another name for Orrery's service. To do that, it moves
Moonraker's own service file aside, to `/etc/systemd/system/moonraker.service.orrery-saved`.

To keep Moonraker's service file, add `--no-alias-moonraker`. Later runs of setup keep that
choice. You can also set it in `orrery.toml`; see
[Moonraker compatibility](configuration/access.md#moonraker-compatibility).

## Installing by hand

These are the installer's steps, for a printer where the installer can't run. Each release has
one archive for each kind of CPU:

| Archive | For |
|---|---|
| `orrery-aarch64.tar.gz` | Single-board computers with a 64-bit ARM system, which most current ones have |
| `orrery-armv7.tar.gz` | Single-board computers with a 32-bit ARM system |
| `orrery-x86_64.tar.gz` | PCs |

To see which one the printer needs, run `uname -m`. It prints `aarch64`, `armv7l`, or `x86_64`.

1. Download the archive and the release's checksums into `~/orrery`, check the archive, and
   unpack it:

   ```
   mkdir -p ~/orrery && cd ~/orrery
   curl -LO https://github.com/ressu/orrery/releases/latest/download/orrery-<cpu>.tar.gz
   curl -LO https://github.com/ressu/orrery/releases/latest/download/SHA256SUMS
   sha256sum --check --ignore-missing SHA256SUMS
   tar -xzf orrery-<cpu>.tar.gz && rm orrery-<cpu>.tar.gz SHA256SUMS
   ```

   `<cpu>` is `aarch64`, `armv7`, or `x86_64`. `sha256sum` prints `OK` for the archive.
2. To bring Moonraker's data along, follow
   [Importing Moonraker's data](#importing-moonrakers-data) now.
3. Set up Orrery's service, and switch from Moonraker to Orrery:

   ```
   sudo ~/orrery/orrery setup --migrate
   ```

   Setup stops and disables Moonraker, then starts Orrery on Moonraker's port.

Without `--migrate`, setup leaves Moonraker running, and says how to switch. It moves
Moonraker's service file only with `--migrate` or `--force`.

### Trying Orrery next to Moonraker

To set up Orrery's service but leave Moonraker as it is, run setup without `--migrate`, and
keep Moonraker's service file:

```
sudo ~/orrery/orrery setup --no-alias-moonraker
```

Both programs use port 7125 by default, so to run them at the same time, give Orrery another
port in `orrery.toml`:

```toml
[server]
port = 7126
```

To switch to Orrery later, run `sudo ~/orrery/orrery setup --migrate`.

## Importing Moonraker's data

`orrery-import` copies Moonraker's settings, webcams, print history, job queue, API key, and
users into Orrery's database. It reads `moonraker-sql.db`, which Moonraker has used since v0.9.0.
An older Moonraker keeps its data in `moonraker.db/`: update Moonraker and start it once, and it
moves the data into `moonraker-sql.db`. The installer's
`--import` does these steps for you:

1. Stop Moonraker and Orrery.
2. See what the import would change:

   ```
   ~/orrery/orrery-import --dry-run ~/printer_data/database/moonraker-sql.db
   ```

3. Import, then start Orrery:

   ```
   ~/orrery/orrery-import ~/printer_data/database/moonraker-sql.db
   ```

The import never changes Moonraker's database. Running it again changes nothing, and Orrery
keeps the print jobs it recorded itself. To start over, add `--replace`: it first removes
everything earlier imports wrote.

Imported users log in with the same password as before. Until you give them roles, they're
`admin`, as every Moonraker user is; see [Users and logins](configuration/access.md#users-and-logins).
For what the import leaves out, see "Your data" in
[Orrery compared with Moonraker](comparison.md#what-to-look-out-for).

## Updating Orrery

Fluidd and Mainsail list Orrery beside Klipper, and update it in the same way. Orrery restarts
at the end, so the page reconnects. If a print is running, the update waits for it to end.
This needs the update manager, which most printers that ran Moonraker have set up already; see
[The update manager](configuration/update-manager.md).

You can also update from a shell instead:

```
~/orrery/orrery self-update
```

To install a given release, add `--version`, such as `--version v0.2.0`. Without the update
manager, `self-update` replaces the programs itself, and prints the command that restarts
Orrery.

### The check before an update

Before a new version takes over, the update runs `orrery preflight` from it. The check makes
sure the new version runs on this CPU, reads the config, and can open the database. If it
fails, the old version stays, and the update says why.

### Rolling back

The update keeps the programs it replaced in `~/orrery/.previous/`. To go back to them, use
Rollback in Fluidd or Mainsail, or run:

```
~/orrery/orrery rollback
```

If the new version doesn't run at all, run the old one's rollback instead:

```
~/orrery/.previous/orrery rollback
```

A build of your own doesn't update itself. To follow a fork's releases, see
[Orrery's own entry](configuration/update-manager.md#orrerys-own-entry).

## Removing old logs

Setup can have the printer's user remove old files in the logs folder, `~/printer_data/logs`.
It's off unless you turn it on, because it removes every program's files there, not only logs.
Some tools keep results in the logs folder, such as older versions of Shake&Tune, in
`ShakeTune_results`. Move what you want to keep before you turn the cleanup on.

To turn it on, set this in `orrery.toml` and run setup again:

```toml
[setup]
log_cleanup = true
```

Or run `sudo ~/orrery/orrery setup --log-cleanup`. `--no-log-cleanup` turns it off again, and
later runs of setup keep the choice. A flag wins over `orrery.toml`.

Once a day, the user's `systemd-tmpfiles-clean.timer` then removes the files that nothing has
written, read, or renamed for 14 days. The setting is in
`~/.config/user-tmpfiles.d/orrery.conf`. So that the timer runs while nobody is logged in, setup
turns on lingering for the user, and enables the timer, which Debian leaves off. Turning the
cleanup off, or `setup --remove`, removes the file and leaves both on.

### Lingering and the timer

Lingering keeps the user's own services and timers running while nobody is logged in. Other
programs may need it too, such as an agent with `systemd_scope = "user"`, so setup never turns
it off. If nothing needs it any more, turn it off with:

```
sudo loginctl disable-linger <user>
```

`<user>` is the printer's user. The user's services then stop when the user logs out.

The timer applies every file in `~/.config/user-tmpfiles.d`, and does nothing without one. To
turn it off, run as the printer's user:

```
systemctl --user disable --now systemd-tmpfiles-clean.timer
```

## Going back to Moonraker

```
sudo ~/orrery/orrery setup --remove
```

This removes Orrery's service and its log cleanup's file, and puts Moonraker's service file
back if setup moved it. It prints the command that starts Moonraker again. Orrery's programs
and database stay in `~/orrery` and `~/printer_data/database`, so you can switch back later.
Lingering and the user's cleanup timer stay on; see
[Lingering and the timer](#lingering-and-the-timer).

## Running setup again

Run setup again after you move Orrery, or to give Orrery permissions it didn't have before,
such as installing Debian packages for the update manager:

```
sudo <path to orrery> setup
```

`<path to orrery>` is the installed program, such as `~/orrery/orrery`. Setup writes the service
to run the program it was started from. To see what's in place without changing anything, run
`<path to orrery> setup --check`. It also asks the running Orrery what polkit allows it, which
is what the rule is for. Only root can read the rule itself, so to compare it with what setup
writes, run the check with `sudo`.
