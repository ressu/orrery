# Orrery

Orrery is an API server for Klipper that can take Moonraker's place. Fluidd, Mainsail, and
KlipperScreen work with it as they do with Moonraker. It reads your `moonraker.conf`, and it
can import Moonraker's print history, job queue, and settings.

Orrery is experimental. It's written in Rust, and it runs on the same computers as Klipper:
single-board computers with a 32-bit or 64-bit ARM CPU, and PCs. Its long-term goal is to try
out ideas Moonraker doesn't have, such as permissions for each user.

## What works

- Printing, files, uploads and downloads, print history, the job queue, webcams, and the
  database that Fluidd and Mainsail keep their settings in.
- Roles that set what each client may do: `viewer`, `operator`, and `admin`. A client gets its
  role from its network, an API key, or a user login. Moonraker's trusted networks and Fluidd's
  API key work as before.
- Spoolman, and the update manager in Fluidd and Mainsail.
- Agents: programs that add features to Orrery over a local socket. Spoolman and the update
  manager are agents.

Some things work differently from Moonraker, and some aren't there yet. See
[Orrery compared with Moonraker](docs/comparison.md) before you switch.

## Installing

Run this on the printer, as the user Moonraker runs as:

```
curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh -s -- --import --migrate
```

The installer:

1. Puts Orrery in `~/orrery`, and checks the download against the release's checksums.
2. Imports Moonraker's print history, job queue, and settings (`--import`).
3. Stops Moonraker and starts Orrery in its place (`--migrate`).

It asks for your password for `sudo` when it sets up Orrery's service. It stops if the printer
is printing. Without `--import` and `--migrate`, it asks you about each one.

For the installer's other options, installing by hand, and trying Orrery next to Moonraker, see
[Installing Orrery](docs/install.md).

## Moving from Moonraker

The installer's `--import` imports Moonraker's data for you. If Orrery is installed already,
import it by hand:

1. Stop Moonraker and Orrery.
2. See what the import would change:

   ```
   ~/orrery/orrery-import --dry-run ~/printer_data/database/moonraker-sql.db
   ```

3. Import, then start Orrery:

   ```
   ~/orrery/orrery-import ~/printer_data/database/moonraker-sql.db
   ```

The import only works with Moonraker v0.8 or later. For what it brings over, see
[Importing Moonraker's data](docs/install.md#importing-moonrakers-data).

## Updating

Update Orrery from Fluidd or Mainsail, as you update Klipper. Read the [changelog](CHANGELOG.md)
first.

Orrery tries to make sure a new version works before it takes over. If an update goes wrong,
use Rollback in Fluidd or Mainsail, or run:

```
~/orrery/orrery rollback
```

You can also update from a shell instead:

```
~/orrery/orrery self-update
```

For the details, see [Updating Orrery](docs/install.md#updating-orrery).

## Going back to Moonraker

```
sudo ~/orrery/orrery setup --remove
```

This removes Orrery's service and puts Moonraker's back. It prints the command that starts
Moonraker again.

## Configuration

Orrery reads `moonraker.conf` and uses the settings it understands, so a printer that ran
Moonraker works without changes. Orrery's own settings, and any new ones you add, go in
`orrery.toml`, next to `moonraker.conf`. See [Configuring Orrery](docs/configuration/README.md).

## More documentation

| Page | What it covers |
|---|---|
| [Installing Orrery](docs/install.md) | The installer, installing by hand, importing, updating, and removing |
| [Configuring Orrery](docs/configuration/README.md) | `orrery.toml`, and the settings Orrery reads from `moonraker.conf` |
| [Orrery compared with Moonraker](docs/comparison.md) | What you lose, gain, and should look out for when you switch |
| [Regressions](docs/regressions.md) | Each difference from Moonraker that a conformance case checks |
| [Moonraker oddities Orrery keeps](docs/oddities.md) | Moonraker behaviour that looks odd, kept so clients work |
| [Changelog](CHANGELOG.md) | What changes in each release |
| [Developing Orrery](docs/development.md) | Building, testing, and making a release |

## The name

An orrery is a clockwork model of the planets moving around the sun. Orrery started out as a
placeholder name for the project, and the name grew on me, so it stays.

## License

Orrery is a Moonraker-compatible API server, heavily inspired by
[Moonraker](https://github.com/Arksine/moonraker). Moonraker is copyright Eric Callahan
(Arksine) and its contributors.

Orrery is licensed under the GNU General Public License, version 3 only, as Moonraker is. See
[LICENSE](LICENSE).
