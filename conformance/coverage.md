# Coverage checklist

This checklist lists every Moonraker method, notification, and HTTP route that Fluidd uses, and
whether a conformance case covers it. It comes from Fluidd's `src/api/socketActions.ts`,
`src/store/socket/actions.ts`, and its HTTP calls. Update it when you add cases.

- **Covered:** at least one case calls the method or waits for the notification.
- **To do:** in scope, no case yet.
- **Stub:** out of scope for orrery's core. orrery needs only an answer that keeps frontends
  working, such as an empty list. See `docs/regressions.md`.

## Connect and boot

| Method | Status | Cases |
|---|---|---|
| `server.info` | Covered | `server/info`, `server/info_no_klippy`, many others |
| `server.config` | Covered | `server/config` |
| `server.connection.identify` | Covered | `server/identify` |
| `server.websocket.id` | Covered | `server/websocket_id` (not used by Fluidd) |
| `printer.info` | Covered | `printer/info`, `printer/startup`, `printer/error_state` |
| `server.restart` | Done, not covered | Restarts the server, so the runner would need to reconnect; checked by hand |
| `server.logs.rollover` | Covered | `server/logs_rollover`; Klipper's log only without a service, since the suite has none |

| Notification | Status | Cases |
|---|---|---|
| `notify_klippy_ready` | Covered | `printer/restart`, `printer/startup` |
| `notify_klippy_shutdown` | Covered | `printer/shutdown`, `printer/emergency_stop` |
| `notify_klippy_disconnected` | Covered | `printer/disconnect` |

## Printer

| Method | Status | Cases |
|---|---|---|
| `printer.objects.list` | Covered | `printer/objects_list` |
| `printer.objects.query` | Covered | `printer/objects_query`, `printer/objects_query_errors` |
| `printer.objects.subscribe` | Covered | `printer/subscribe`, `printer/subscribe_replace` |
| `printer.gcode.script` | Covered | `printer/gcode_script` |
| `printer.gcode.help` | Covered | `printer/gcode_help` |
| `printer.emergency_stop` | Covered | `printer/emergency_stop` |
| `printer.restart` | Covered | `printer/restart` |
| `printer.firmware_restart` | Covered | `printer/firmware_restart` |
| `printer.query_endstops.status` | Covered | `printer/query_endstops` |
| `printer.print.start` | Covered | `printer/print_control`, `printer/start_print_leading_slash` |
| `printer.print.pause` | Covered | `printer/print_control` |
| `printer.print.resume` | Covered | `printer/print_control` |
| `printer.print.cancel` | Covered | `printer/print_control`, `history/cancelled` |
| `server.gcode_store` | Covered | `server/gcode_store` |
| `server.temperature_store` | Covered | `server/temperature_store` |

| Notification | Status | Cases |
|---|---|---|
| `notify_status_update` | Covered | `printer/subscribe`, `printer/subscribe_replace` |
| `notify_gcode_response` | Covered | `printer/gcode_script`, `server/gcode_store` |

## Files

| Method or route | Status | Cases |
|---|---|---|
| `server.files.roots` | Covered | `files/roots` |
| `server.files.list` | Covered | `files/list` |
| `server.files.get_directory` | Covered | `files/list`, `files/directories` |
| `server.files.post_directory` | Covered | `files/directories`, `files/notifications` |
| `server.files.delete_directory` | Covered | `files/directories`, `files/delete_nonempty` |
| `server.files.delete_file` | Covered | `files/directories`, `files/notifications` |
| `server.files.move` | Covered | `files/directories`, `files/notifications` |
| `server.files.copy` | Covered | `files/directories` |
| `server.files.zip` | Covered | `files/zip` |
| `server.files.metadata` | Covered | `files/metadata`, `files/metadata_plain` |
| `server.files.metascan` | Covered | `files/metascan` |
| `server.files.thumbnails` | Covered | `files/metadata` (not used by Fluidd) |
| `POST /server/files/upload` | Covered | `files/upload_download`, `files/upload_checksum`, `files/upload_print` |
| `GET /server/files/<root>/<path>` | Covered | `files/upload_download`, `files/metadata`, `files/zip` |

| Notification | Status | Cases |
|---|---|---|
| `notify_filelist_changed` | Covered | `files/notifications` |
| `notify_metadata_update` | To do | |

Metadata is covered for PrusaSlicer and for files with no slicer comments. Other slicers
(SuperSlicer, OrcaSlicer, Cura, Simplify3D, and Klipper Estimator) are still to do. Each needs a
fixture in `fixtures/files/`.

## Database

| Method | Status | Cases |
|---|---|---|
| `server.database.list` | Covered | `database/round_trip`, `database/protected`, `database/backup` |
| `server.database.get_item` | Covered | `database/round_trip`, `database/namespaces`, `database/missing` |
| `server.database.post_item` | Covered | `database/round_trip`, `database/namespaces`, `database/protected` |
| `server.database.delete_item` | Covered | `database/round_trip`, `database/missing` |
| `server.database.post_backup` | Covered | `database/backup` |
| `server.database.delete_backup` | Covered | `database/backup` |
| `server.database.restore` | To do | Restarts the server, so the runner needs to reconnect |
| `server.database.compact` | To do | |

## History and job queue

| Method | Status | Cases |
|---|---|---|
| `server.history.list` | Covered | `history/job_lifecycle`, `history/http`, `history/sequence` |
| `server.history.get_job` | Covered | `history/job_lifecycle`, `history/http` |
| `server.history.delete_job` | Covered | `history/job_lifecycle`, `history/sequence` |
| `server.history.totals` | Covered | `history/job_lifecycle`, `history/http` |
| `server.history.reset_totals` | Covered | `history/job_lifecycle`, `history/sequence` |
| `server.job_queue.status` | Covered | `job_queue/queue`, `job_queue/start`, `job_queue/automatic` |
| `server.job_queue.post_job` | Covered | `job_queue/queue`, `job_queue/notifications` |
| `server.job_queue.delete_job` | Covered | `job_queue/queue`, `job_queue/notifications` |
| `server.job_queue.pause` | Covered | `job_queue/queue` |
| `server.job_queue.start` | Covered | `job_queue/start`, `job_queue/automatic` |
| `server.job_queue.jump` | Covered | `job_queue/automatic` |

| Notification | Status | Cases |
|---|---|---|
| `notify_history_changed` | Covered | `history/job_lifecycle`, `history/cancelled` |
| `notify_job_queue_changed` | Covered | `job_queue/notifications` |

## Authorization

The runner can't send credentials yet, so this section waits on runner support for logins,
tokens, and API keys.

| Method | Status |
|---|---|
| `access.info` | To do |
| `access.login` | To do |
| `access.logout` | To do |
| `access.refresh_jwt` | To do |
| `access.oneshot_token` | To do |
| `access.get_user` | To do |
| `access.post_user` | To do |
| `access.delete_user` | To do |
| `access.users.list` | To do |
| `access.user.password` | To do |
| `access.get_api_key` | To do |
| `access.post_api_key` | To do |

| Notification | Status |
|---|---|
| `notify_user_created` | To do |
| `notify_user_deleted` | To do |
| `notify_user_logged_out` | To do |

## Machine, webcams, and announcements

| Method | Status | Cases |
|---|---|---|
| `machine.system_info` | Covered | `machine/system_info` (known regression: `no-python-info`) |
| `machine.proc_stats` | Covered | `machine/proc_stats` |
| `machine.reboot`, `machine.shutdown` | Done, not covered (guarded; never run in tests) |
| `machine.services.restart`, `.start`, `.stop` | Covered: the refusals only (`machine/services`); actions checked by hand |
| `machine.peripherals.usb`, `.serial`, `.video`, `.canbus` | To do |
| `server.webcams.list`, `.get_item`, `.post_item`, `.delete_item`, `.test` | Covered | `webcam/config`, `webcam/saved` |
| `server.announcements.list`, `.dismiss` | To do |
| `server.sensors.list` | Stub |

| Notification | Status | Cases |
|---|---|---|
| `notify_proc_stat_update` | Covered | `machine/proc_stats` |
| `notify_cpu_throttled` | Written | Needs a Raspberry Pi; not testable in the suite |
| `notify_webcams_changed` | Covered | `webcam/saved` |
| `notify_announcement_update`, `_dismissed`, `_wake` | To do |
| `notify_service_state_changed` | To do |
| `notify_sensor_update` | Stub |

## Out of scope

Fluidd calls these, but they belong to components orrery leaves out for now. Each one needs at
most a stub answer:

- `machine.update.*` (update manager): `machine.update.status` should answer with no updates, so
  Fluidd's software panel doesn't show an error. Notifications: `notify_update_response`,
  `notify_update_refreshed`.
- `machine.device_power.*` (power devices), `notify_power_changed`.
- `machine.timelapse.*` (timelapse plugin), `notify_timelapse_event`.
- `server.spoolman.*`, `notify_active_spool_set`, `notify_spoolman_status_changed`.
- `server.analysis.*` (Klipper Estimator analysis).
