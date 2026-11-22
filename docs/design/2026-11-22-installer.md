# A one-line installer

**Proposal (2026-11-22):** a shell script installs orrery from a release with one command, and
can import Moonraker's data and switch the printer over in the same run:

```
curl -fsSL https://github.com/ressu/orrery/releases/latest/download/install.sh | sh -s -- --import --migrate
```

Without flags, it installs and sets up the service, then asks on the terminal whether to import
and whether to switch over (user, 2026-11-22). `--import` matters most: it replaces stopping
Moonraker, copying its database, and running `orrery-import` by hand (user, 2026-11-22).

**Status (2026-11-22):** approved, with the open questions answered (below), and built:
`scripts/install.sh`, its copy in each release, and the release workflow's test. Tested on a
PC with stand-ins for `sudo` and `systemctl`: the questions on a terminal, the import from a
test database, setup's flags, a bad checksum, an existing install, and an old `data.mdb`. No
printer has run it yet.

## What exists today

The README's install steps, in [Installing](../../README.md#installing), take five commands
for the download alone, then [Moving from Moonraker](../../README.md#moving-from-moonraker)
takes five more, and `sudo ~/orrery/orrery setup --migrate` one more. Every step is one the
script can do: none of them needs a choice beyond "import?" and "switch now?".

## Where the script lives

- **`scripts/install.sh`** in the repository, POSIX `sh`, checked by `shellcheck` in CI with
  the other scripts. Raspberry Pi OS and Armbian have `curl`, `tar`, and `sha256sum`.
- **Each release has a copy** as the asset `install.sh`, so
  `releases/latest/download/install.sh` is the latest release's script, and it installs that
  same release. A link to the repository's `main` branch could run a script newer than the
  release it downloads.
- **A fork's releases install from the fork.** The release workflow writes
  `GITHUB_REPOSITORY` into its copy of the script, as `dist.sh` writes it into
  `release_info.json`.

## What it does

The script runs as the printer's user, the one orrery will run as. It refuses to run as root,
since setup takes that user from `sudo`. It asks for `sudo` only for setup, and to stop and
start Moonraker for the import.

1. **Checks and questions,** before anything changes:
   - the CPU, from `uname -m`, has an archive (`aarch64`, `armv7`, or `x86_64`);
   - `curl`, `tar`, `sha256sum`, and `systemctl` are there;
   - `~/orrery` holds no orrery yet. An existing install stops the script, with the command
     that updates it, `~/orrery/orrery self-update` (user, 2026-11-22);
   - the import and switch-over questions, so that stopping at one leaves nothing behind.
2. **Download:** the archive and `SHA256SUMS`, into a new hidden folder in `~/orrery`. The
   archive must match its checksum. The script unpacks it there, then moves the programs into
   `~/orrery`, on the same file system, so a failed download leaves nothing behind.
3. **Preflight:** `~/orrery/orrery preflight`, which checks that orrery runs on this CPU and
   reads the printer's config.
4. **Import,** with `--import` or a "yes":
   - The script checks that the printer isn't printing, by asking Moonraker's
     `print_stats`. A print or a pause stops the script before Moonraker stops.
   - It stops Moonraker, copies `moonraker-sql.db` into the hidden folder, and runs
     `orrery-import` on the copy. With `--migrate`, Moonraker stays stopped; without, the
     script starts it again.
   - Only `data.mdb`, from Moonraker before v0.8: the script says it can't import data from
     that Moonraker, and asks whether to install without the import, "no" by default
     (user, 2026-11-22). With no terminal, it stops. No database at all, with `--import`,
     gets the same question: most likely `-d` names the wrong folder.
5. **Setup:** `sudo ~/orrery/orrery setup`, with `--migrate` for `--migrate` or a "yes".
   `--migrate` stops Moonraker too, so the print check runs again first. A Moonraker that runs
   but doesn't answer, as with a login it requires, can't say whether it prints: the script
   asks whether to stop it anyway, "no" by default.
   `--no-alias-moonraker` passes through. Setup writes the service, and `--migrate` switches
   from Moonraker, as it does by hand.
6. **The end:** what runs now, and the commands to update orrery and to go back to Moonraker.
   The hidden folder is removed.

## Flags and questions

| Flag | Without it |
|---|---|
| `--import`, `--no-import` | Asks, when there's a Moonraker database to import |
| `--migrate`, `--no-migrate` | Asks, when Moonraker is installed; otherwise setup starts orrery |
| `--no-alias-moonraker` | Setup's default: orrery takes the name `moonraker.service` |
| `--version vX.Y.Z` | The script's own release |
| `-d`, `--datapath <folder>` | `~/printer_data` |
| `--no-setup` | Setup runs. With it, the script only installs the programs |

The questions read from `/dev/tty`, since the script itself comes in on standard input. With no
terminal, as in a script, a question without a flag counts as "no", and the script prints the
command that does the rest. The questions follow the project's style: the facts first, the
question alone on the last line, with the default in capitals:

```
orrery can copy Moonraker's print history, job queue, and settings, so they stay.
Moonraker stops while orrery copies its database.
Import Moonraker's data into orrery? [Y/n]
```

## Testing

- `shellcheck`, in CI, as for the other scripts.
- The release workflow runs the script against the archive it just built, in a temporary home
  folder, with `--no-setup --no-import`: the checksum, the unpacking, and the preflight. An
  environment variable points the script at local files in place of GitHub; it's for tests,
  and the README doesn't mention it.
- On pink-0, with Moonraker put back first: the whole run, `--import --migrate`, then
  without flags, answering the questions.

## Answered (user, 2026-11-22)

- **Reinstalling over an existing `~/orrery`:** the script stops, and points to
  `orrery self-update`.
- **`data.mdb`:** no import from Moonraker before v0.8. The script says so, and offers to stop.
- **`sudo`:** a line before each use says why the script needs it.

**Not in the questions:** a count of print jobs. Counting needs `sqlite3`, which printers
don't always have, and reading Moonraker's database while it runs isn't safe. The question
names what the import copies instead.
- **The `sudo` prompt** comes from `sudo` itself, in the middle of the script's output. A line
  before it says why the script needs it (user, 2026-11-22).
