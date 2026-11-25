# Moonraker oddities orrery keeps

This file lists Moonraker behaviour that looks like a bug, but that orrery copies on purpose so
clients see the same thing. Each entry says what happens, why, and what a fix would cost.
Differences from Moonraker go in [regressions.md](regressions.md) or [comparison.md](comparison.md) instead.

## Entry format

```markdown
### <id>: <short title>

- **Area:** the component
- **What you see:** the behaviour, as a user meets it
- **Why:** what Moonraker and the clients do that causes it
- **Possible fix:** what orrery could do instead, and the catch
```

## Entries

### queue-reorder-times: reordering the job queue resets every job's time

- **Area:** job queue
- **What you see:** after you drag jobs into a new order in Fluidd, every job in the queue shows
  the same "time added": the time of the move.
- **Why:** the queue has no request to move a job, apart from `jump`, which moves one job to the
  front. Fluidd reorders by sending the whole list again with `reset: true`
  (`src/components/widgets/job-queue/JobQueueBrowser.vue`). Moonraker then clears the queue and
  creates each job again, with a new `job_id` and the current time.
- **Possible fix:** when a reset list names a file that was already queued, reuse that job and
  its time. When the same file is queued more than once, orrery can only guess which time goes
  with which copy. The fix would be a difference from Moonraker, with an entry in
  `regressions.md`.
