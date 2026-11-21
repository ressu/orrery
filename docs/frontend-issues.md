# Issues in frontends and other clients

This file lists bugs in frontends and other clients, such as Fluidd and Home Assistant, that
testing orrery has turned up. Most of them affect Moonraker users too, but orrery's roles make
them easier to hit. Each entry is meant to become an upstream bug report or fix. When one is
filed, add its link; when it's fixed upstream, remove the entry.

## Entry format

```markdown
### <id>: <short title>

- **Frontend** or **Client:** the program and the version tested
- **What happens:** what the user sees
- **Cause:** the code that does it, with `file:line`
- **Fix:** what would fix it
- **Upstream:** the bug report or pull request, once there is one
```

## Fluidd

### fluidd-auth-nav: the Authentication link shows without the Authentication section

- **Frontend:** Fluidd 1.38.1, with orrery on bluev2, 2026-11-21.
- **What happens:** the settings page lists "Authentication" in its navigation, but the page
  has no Authentication section, so the link goes nowhere.
- **Cause:** the navigation entry is always visible (`src/components/layout/AppSettingsNav.vue:40`,
  `visible: true`). The section is shown only when the server reports the `authorization`
  component (`src/views/Settings.vue:15,79`). orrery doesn't report it yet. Moonraker always
  does, so Moonraker users don't see this.
- **Fix:** make the navigation entry's `visible` follow the same check, as the timelapse and
  MMU entries do in the same list.
- **Upstream:** not filed yet.

### fluidd-save-error: a refused save shows no error

- **Frontend:** Fluidd 1.38.1, with orrery on bluev2, 2026-11-21.
- **What happens:** saving a file in the editor that the server refuses, such as `printer.cfg`
  from an operator, shows no error. The editor gives no sign that the file wasn't saved.
- **Cause:** `handleSaveFileChanges` awaits `uploadFile` without catching its error
  (`src/components/widgets/filesystem/FileSystem.vue:993-1000`), and `uploadFile` only
  cleans up in `finally` (`src/mixins/files.ts:200-244`). The rejected promise goes nowhere,
  so no message is shown, and the lines after the upload, such as the restart the user asked
  for, don't run. `handleSaveAsFileChanges` (`FileSystem.vue:976`) calls the same code.
- **Fix:** catch the error in `handleSaveFileChanges`, show the server's message, keep the
  editor open with the changes, and skip the restart.
- **Upstream:** not filed yet.

### fluidd-upload-error: a refused upload shows no error

- **Frontend:** Fluidd 1.38.1, with orrery on bluev2, 2026-11-21.
- **What happens:** uploading a file the server refuses, such as any upload from a viewer,
  shows no error. The upload's progress entry goes away as if it had finished.
- **Cause:** `uploadFiles` catches each upload's error and only writes it to the browser's
  console (`src/mixins/files.ts:299-303`, `consola.error('[FileUpload] file', error)`).
  `uploadFile` removes the progress entry in `finally` (`src/mixins/files.ts:241-243`), so
  nothing on the screen is left to show the failure. It's the same gap as
  `fluidd-save-error`, in the other path to the same upload.
- **Fix:** show the server's message for a failed upload, for example as the socket errors are
  shown (`src/store/socket/actions.ts:297-308`), and mark the upload as failed instead of
  removing it.
- **Upstream:** not filed yet.

### fluidd-viewer: Fluidd offers a viewer actions it can't take

- **Frontend:** Fluidd 1.38.1, with orrery on bluev2, 2026-11-21.
- **What happens:** with the viewer role, Fluidd still shows every button: print, pause,
  upload, edit, the console, and so on. Each one fails when clicked, some without saying so.
- **Cause:** Moonraker has no roles, so Fluidd assumes a connected client may do everything.
  It learns nothing about its rights from the server.
- **Fix:** needs a way for the server to tell a client its role, and Fluidd to use it. orrery
  could report the role in `access.info` or `access.get_user`; that would be orrery's own
  addition, and Fluidd would need to read it. Worth raising with Fluidd before building.
- **Upstream:** not filed yet.

## Home Assistant

### ha-key-setup-only: an API key can be set only when the printer is added

- **Client:** the Moonraker integration (`moonraker-home-assistant`), tested with orrery on
  bluev2, 2026-11-21.
- **What happens:** an integration that's already set up has no way to add or change its API
  key. To use a key, the printer has to be removed from Home Assistant and added again, which
  loses its entities' settings.
- **Cause:** the config flow has only the first setup step, `async_step_user`
  (`custom_components/moonraker/config_flow.py:34`), and no reconfigure or options step for the
  key. The key is read from the entry's data at startup (`custom_components/moonraker/__init__.py:65`).
- **Fix:** a reconfigure step that asks for the URL, port, and API key again.
- **Upstream:** not filed yet.
