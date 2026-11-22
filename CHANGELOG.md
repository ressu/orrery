# Changelog

What changes in each release of orrery. Read it before you update, so you know what to expect.

Fluidd and Mainsail link to the release page on GitHub, which shows the same notes.

## Unreleased

The first release. orrery is a Moonraker-compatible API server for Klipper and Kalico: Fluidd,
Mainsail, and KlipperScreen connect to it as they do to Moonraker. It reads your
`moonraker.conf`, and `orrery-import` copies your print history, job queue, and settings from
Moonraker's database. Spoolman and the update manager work as in Moonraker, and the update
manager updates orrery itself from these releases. `docs/regressions.md` lists what still works
differently from Moonraker.
