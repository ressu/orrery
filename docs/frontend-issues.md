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
  component (`src/views/Settings.vue:15,79`). Since 2026-11-21 orrery reports it, so this
  shows only with a server that doesn't, such as orrery before logins. Moonraker always
  does, so Moonraker users don't see this.
- **Fix:** make the navigation entry's `visible` follow the same check, as the timelapse and
  MMU entries do in the same list.
- **Upstream:** not filed yet.

### fluidd-auth-nonadmin: the Authentication settings expect every client to list users

- **Frontend:** Fluidd 1.38.1, from its source, 2026-11-21. Not yet seen on bluev2.
- **What happens (expected):** for a client that isn't an admin, the Authentication section
  loads, but orrery refuses its list of users and its API key. Fluidd may show the refusals as
  errors, and the section shows no users.
- **Cause:** `auth/init` asks for the current user, the user list, and the API key together,
  and needs all three to succeed (`src/store/auth/actions.ts:20-31`). It runs whenever the
  server reports the `authorization` component (`src/globals.ts:271`). Moonraker lets any
  authorized client list users and read the API key, so Fluidd never expects a refusal.
- **Fix:** load the list and the key only when the section is opened, and hide the parts the
  server refuses.

### fluidd-login-error: a refused login always says "Invalid credentials"

- **Frontend:** Fluidd 1.38.1, from its source and on bluev2, 2026-11-21.
- **What happens:** when orrery locks an address out after `max_login_attempts`, Fluidd still
  says "Invalid credentials". orrery's reason, "Too many failed logins from this address. Try
  again in 15 minutes.", never shows, so the user doesn't know they're locked out or for how
  long. The same goes for every other reason, such as a user that `orrery.toml` gives no role.
- **Cause:** the login form catches any error and shows a fixed text
  (`src/components/common/Login.vue:24-28,149-150`). Fluidd's error toasts show a server's
  message only once the connection is ready (`src/store/socket/actions.ts:297-308`), and at
  the login screen it isn't. So no reply from orrery can reach the user.
- **Fix:** show the server's message in the login form's alert, at least for a lockout. orrery
  keeps "Wrong user name or password" for a wrong name or password, so the message tells a
  guesser nothing more.
- **Upstream:** not filed yet. It waits until we start sending orrery's patches upstream
  (user, 2026-11-21).

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
