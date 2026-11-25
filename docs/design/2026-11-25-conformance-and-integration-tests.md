# Conformance cases and integration tests

**Proposal (2026-11-25), for later.** Nothing is decided or built yet.

## The problem

The conformance suite was made to check that Orrery answers as Moonraker does, with Moonraker as
the reference: a case is right when it passes against Moonraker. Over time it also became
Orrery's integration tests. On 2026-11-25, 25 of its 97 cases ran against Orrery only
(`targets: [orrery]`):

| Area | Cases | What they test |
|---|---|---|
| `agents/` | 11 | Agents, their keys, history fields, the Spoolman and update agents |
| `auth/` | 9 | Roles, keys, proxy logins, lockout, `force_logins` |
| `actions/` | 3 | Actions and buttons |
| `update/` | 1 | The update agent |
| `files/` | 1 | Orrery's file watcher |

Every case that writes `orrery.toml` is one of them, so the shared cases still check only what
Moonraker has.

Mixing the two causes trouble:

- **No reference.** An Orrery-only case is right only because we wrote it so; the design note
  is the spec.
- **The case format grows** to express Orrery's features: agent steps, `save`, `disk`.
- **They run outside `cargo test`,** and a failure is a diff, not a test to step through.
- **`docs/regressions.md` gets muddled.** It lists the differences that conformance cases
  check, but many of its entries are checked only by Orrery-only cases. That's how Orrery's
  own features ended up described as regressions.

## Proposal

1. **A conformance case runs against Moonraker.** `targets: [orrery]` goes away, and the runner
   gets no Orrery-only features. Snapshots, `ignore` paths, transports, `known_regression`, and
   the fake Klippy stay: they're what comparing two servers needs.
2. **Integration tests use `cargo test`,** like `crates/orrery-spoolman/tests/agent.rs` and
   `crates/orrery-agent/tests/client.rs` already do. A test-support library, taken from the
   conformance crate, starts Orrery with a temporary data folder and `orrery.toml`, and has the
   fake Klippy (already the `fake-klippy` crate), websocket and HTTP clients, and a test agent.
3. **Port the 25 cases** one area at a time, to `crates/orrery/tests/` and the agents' crates.
   Much of `update/agent` may fit the updater's unit tests instead.
4. **`docs/regressions.md` keeps only what a shared case checks.** The parts of an entry that
   only an Orrery-only case checks move to `docs/comparison.md`. For example, `auth-roles` stays,
   since `auth/untrusted` and others run against both servers, while `history-aux-fields` and
   `spoolman-agent` move.

## Open questions

- Where the test-support library lives: a new crate, or the conformance crate as a library.
- Whether to port all 25 cases before the rule takes effect, or stop adding Orrery-only cases
  now and port as each area changes.
