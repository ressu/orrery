# Import fixtures

`seed.db` is a `moonraker-sql.db` written by upstream Moonraker. The import's unit tests in
`crates/orrery/src/import/mod.rs` read it. It holds:

- Fluidd settings and a `seed` namespace with every value type Moonraker stores
- one webcam, added through `server.webcams.post_item`
- two print history jobs (one completed, one cancelled) and their totals
- Moonraker's own `database`, `moonraker`, and `gcode_metadata` namespaces
- the `_API_KEY_USER_` user Moonraker always creates
- two local users, `alice` (password `secret1`) and `bob` (password `secret2`)

The case `conformance/cases/import/seed.yaml` builds it. To build it again, run the case against
the reference and keep its data folder:

```
just conformance reference --filter import/seed --keep-all
```

The runner prints the kept folder. Copy `database/moonraker-sql.db` from it to `seed.db`, then
update the counts in the import tests.
