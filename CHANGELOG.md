# Changelog

What changes in each release of orrery. Read it before you update, so you know what to expect.

Fluidd and Mainsail link to the release page on GitHub, which shows the same notes.

## 0.1.1 (2026-11-22)

### New

- `sudo orrery setup --alias-moonraker` makes `moonraker.service` another name for orrery's
  service. Services that start or wait for Moonraker, such as aldis, then get orrery, and no
  longer start Moonraker and stop orrery. If Moonraker's own service file is in the way,
  setup changes nothing unless you add `--force`. `--force` disables Moonraker and moves its
  file aside, and `sudo orrery setup --remove` puts it back.

## 0.1.0 (2026-11-22)

First release.
