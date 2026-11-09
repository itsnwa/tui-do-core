# API v0

Base URL: the contents of `~/.tui-do/core/endpoint` (default `http://127.0.0.1:47821`).

Every route except `/v0/health` needs a token, sent as `Authorization: Bearer <token>` or as a `?token=` query parameter (browsers can't set headers on WebSockets): the core's own token from `~/.tui-do/core/token`, or a [paired client's](#paired-clients) token. Errors are JSON: `{ "error": "message" }`.

## Session object

```json
{
  "id": "my-task",
  "alive": true,
  "exitCode": null,
  "agent": "claude",
  "agentState": "waiting",
  "pid": 12345,
  "cols": 120,
  "rows": 40,
  "labels": { "remote": "true" }
}
```

- `id` — chosen by the client; letters, digits, `-`, `_` and `.`, up to 200 characters.
- `exitCode` — set once the session ended; `null` while running or when it was killed by a signal.
- `agent` / `agentState` — for sessions started with `"agent": "claude"`: `idle`, `working` or `waiting`. Comes from Claude Code hooks when the session loads them (see below), otherwise from output timing (2 s of quiet → `waiting`; output more than 500 ms after the last input → `working`).
- `labels` — free-form string tags, stored and returned as given.

## HTTP

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/health` | | `{ name, version, apiVersion, device: { id, name }, home: { id, name, address?, generation } }` (no token needed; see [Finding the home](#finding-the-home)) |
| `GET` | `/v0/device` | | `{ id, name, remoteAccess, tailscaleAddress, platform, version }` |
| `PATCH` | `/v0/device` | `{ name?, remoteAccess? }` | same as `GET`. With `remoteAccess` on, the core also listens on this machine's Tailscale address (same port, token still required); it never listens on other interfaces. |
| `GET` | `/v0/sessions` | | `[Session]` |
| `POST` | `/v0/sessions` | Spawn request | `Session` |
| `GET` | `/v0/sessions/{id}` | | `Session` or 404 |
| `DELETE` | `/v0/sessions/{id}` | | 204; kills the process and forgets the session |
| `POST` | `/v0/sessions/{id}/input` | `{ "data": "ls\n" }` | 204 |
| `POST` | `/v0/sessions/{id}/resize` | `{ "cols": 120, "rows": 40 }` | 204 |
| `GET` | `/v0/sessions/{id}/snapshot` | | `{ data, alive, exitCode, session }` |

### Spawn request

```json
{
  "id": "my-task",
  "argv": ["/bin/zsh", "--login"],
  "cwd": "~/code/project",
  "env": { "PATH": "..." },
  "cols": 120,
  "rows": 40,
  "agent": "claude",
  "labels": {}
}
```

All fields except `id` are optional. An empty `argv` runs the user's default shell as a login shell. An empty `env` inherits the core's environment; a non-empty one replaces it. `cwd` may start with `~/`. If a live session with this id exists it's returned unchanged; a dead one is replaced.

With `"agent": "claude"` the core adds `TUI_DO_CLAUDE_SETTINGS` (a Claude Code settings file with state-reporting hooks) and `TUI_DO_CLAUDE_STATE_FILE` to the environment. Start Claude with `claude --settings "$TUI_DO_CLAUDE_SETTINGS"` to get exact states.

### Snapshots

`data` is the session's screen and scrollback as VT escape sequences. Reset a terminal of the session's size and write `data` into it to reproduce the screen, cursor, styles and terminal modes. The color palette is left out so clients keep their own theme.

## WebSockets

All messages are JSON text frames.

### `GET /v0/sessions/{id}/attach` and `?cols=&rows=`

One session. With `cols` and `rows`, the session takes that size for this connection first (see [Several clients on one session](#several-clients-on-one-session)), so the snapshot arrives at it. The server first sends

```json
{ "type": "snapshot", "data": "<VT>", "session": { ... } }
```

then that session's events (see below) in order, with nothing lost or repeated between the snapshot and the first event. A `snapshot` message always means *reset, then write*: it's sent again if the client fell too far behind to receive every event. The socket closes after `sessionRemoved`.

Client → server:

```json
{ "type": "input", "data": "ls\n" }
{ "type": "resize", "cols": 120, "rows": 40 }
{ "type": "snapshot" }
```

`snapshot` asks for the screen again (answered with a `snapshot` message, in order with the events), for a client that wants its emulator to match the core's exactly, say after another client changed the size.

With `?paced=1`, the client answers every `output` and `snapshot` message with `{ "type": "ack" }`, and the server keeps at most 64 KB unacknowledged, joining what waits into larger messages. When output has waited more than a second, or a client stops acknowledging, the waiting output is dropped and the next `output` message brings the terminal to the current screen, leaving its scrollback alone. A slow link then stays about a second behind at worst. `sessionResized`, `sessionExited` and `sessionRemoved` keep their order with the output before them.

### `GET /v0/events` and `GET /v0/events?output=1`

Every session. The server first sends `{ "type": "sessions", "sessions": [Session] }`, then events. `output` events are only included with `?output=1`; `?output=first` includes only each session's first one. If the client fell behind, the server sends `{ "type": "resync", "sessions": [Session] }`; clients mirroring output should fetch fresh snapshots.

Client → server (one connection keeps keystrokes for all sessions in order):

```json
{ "type": "input", "id": "my-task", "data": "ls\n" }
{ "type": "resize", "id": "my-task", "cols": 120, "rows": 40 }
```

### Events

```json
{ "type": "sessionCreated", "session": { ... } }
{ "type": "output", "id": "my-task", "data": "text with VT sequences" }
{ "type": "sessionExited", "id": "my-task", "exitCode": 0 }
{ "type": "sessionRemoved", "id": "my-task" }
{ "type": "agentState", "id": "my-task", "state": "waiting" }
{ "type": "sessionQuiet", "id": "my-task" }
{ "type": "sessionResized", "id": "my-task", "cols": 120, "rows": 40 }
```

`output` data is always valid UTF-8; a character split across PTY reads is held back until it's complete. `sessionQuiet` fires when a session has produced no output for 2 seconds. `sessionResized` fires when the PTY takes another size (see [Several clients on one session](#several-clients-on-one-session)).

## Notes for clients

- The core doesn't answer terminal queries (device attributes, cursor position reports); the client's terminal emulator does, by sending its replies as input. If several clients are attached, each answers.
- While a full-screen app uses the alternate screen, snapshots don't include the primary screen's scrollback.

### Several clients on one session

A session has one PTY, so one size. The core remembers the size each WebSocket connection asked for (per session), and the PTY follows the client in use:

- A `resize` on a socket applies, and is remembered for that connection.
- `input` from a connection whose size isn't the PTY's gives it its size back first, when it's something typed: anything but escape sequences alone (a terminal's answers to queries are left alone), or a lone Esc.
- When a connection closes, a session that had its size goes back to the size of the connection that used it before, or, with none left, to the size it was started with or last given over HTTP.

Every size change goes out as `sessionResized`. Output always matches the PTY's size, so a client showing a session at another size should size its emulator to the PTY (scaling it to fit, say) until the user types there. Input over HTTP doesn't take the size.

# Workspace

The core serves one workspace: a folder of markdown files with YAML frontmatter (`tasks/`, `projects/`, `plans/`, `daily/`, `weekly/`, `attachments/`). Files are read and written exactly like the original TUI-DO app, so the folder can be shared with it (and synced with iCloud). Until a workspace is set, these routes return `409`.

Writes are atomic and serialized: task IDs are never handed out twice, and creating something whose name is taken picks a free slug (`my-task-2`) instead of overwriting. Errors: `404` not found, `400` invalid input.

Only the [home core](#the-home-role) writes the workspace. Every other core serves a read-only copy: reads work there, and anything that changes the workspace (any non-`GET` request to `/v0/tasks…`, `/v0/projects…`, `/v0/plans…`, `/v0/daily…`, `/v0/weekly…` or `/v0/workspace`) answers `409 { error, home }`, where `home` says where to send it. While the home moves, the home answers those with `503`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/workspace` | | `{ workspacePath }` (`null` if not set) |
| `PUT` | `/v0/workspace` | `{ path }` (absolute) | `{ workspacePath }` |

## Tasks

```json
{
  "slug": "my-task", "id": "26-058", "title": "My task", "state": "in-progress",
  "project": "tui-do", "due": "2026-11-20", "recurrence": null, "plan": "26-058-cosmic-thor",
  "position": -12, "completed": null, "created": "2026-11-07T09:00:10.859Z", "attachments": []
}
```

`state` is `not-started`, `in-progress` or `completed`; `recurrence` is `daily`, `weekly`, `monthly`, `yearly` or `null`. Detail responses add `description` (the markdown body).

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/tasks?project=&state=&due=` | | `[Task]`, sorted: open tasks by due date then position, then completed tasks, most recent first. `due` also matches recurring tasks that fall on that date. |
| `POST` | `/v0/tasks` | `{ title, project?, due?, recurrence?, description? }` | Task + `description` |
| `GET` | `/v0/tasks/lookup?ref=58` | | Task by short ID (`58`, `#58`, `26-058`; bare numbers are this year's) or slug |
| `GET` | `/v0/tasks/{slug}` | | Task + `description` + `content` |
| `PATCH` | `/v0/tasks/{slug}` | any of `{ title, state, project, due, recurrence, plan, description }` (`null` clears `due`, `recurrence`, `plan`) | Task + `description`. Completing a recurring task moves it to its next due date instead. |
| `DELETE` | `/v0/tasks/{slug}` | | Undo token `{ task, planSnapshot, attachmentSnapshots }`; also deletes the task's plan and stored attachments |
| `POST` | `/v0/tasks/restore` | the undo token | Task + `description` |
| `POST` | `/v0/tasks/{slug}/reorder` | `{ direction: "up" \| "down", project? }` | `{ success, error? }` (moves within the same due date) |
| `POST` | `/v0/tasks/{slug}/attachments` | `{ filePath }` or `{ fileName, dataBase64 }` (stored under `attachments/<slug>/`; the request can be up to 32 MB, so a file of about 24 MB) | Task + `description` |
| `POST` | `/v0/tasks/{slug}/attachments/remove` | `{ filePath }` | Task + `description` |
| `GET` | `/v0/tasks/{slug}/attachments/{index}` | | the file at that position in the task's `attachments`, with a `Content-Type` from its extension. Files stored with the task (under `attachments/`) are in the workspace on every machine; an attachment that's a path elsewhere is only found on the machine it's on. |

Stored attachments are kept as absolute paths into the workspace of the home that stored them (`…/attachments/<task>/<file>`). Every core has a copy of that folder. So the MCP tools answer with this machine's path to the same file, `/attachments/{index}` serves it from the home's copy, and `/attachments/remove` takes any machine's path to it.

### Agent workflow

These take a short ID or slug and answer like the TUI-DO MCP tools (business errors are `200` with an `error` or `warning` field).

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `POST` | `/v0/tasks/{ref}/start` | `{ expectedProject? }` | `{ status: "started", task, plan: { slug, content } }`, or a `PROJECT_MISMATCH` warning with alternatives. Reserves a plan slug. |
| `PUT` | `/v0/tasks/{ref}/plan` | `{ content }` | `{ status: "updated", plan: { slug, task } }` |
| `POST` | `/v0/tasks/{ref}/complete` | `{ summary }` | `{ status: "completed" \| "recurring_advanced", task }`; the summary is stored in the plan as `## Completion Summary`. Without a summary: `SUMMARY_REQUIRED`. |

## Projects

`{ slug, title, muted, archived, created, directory }`. `directory` is on the code machine (`codeDevice` in [`/v0/fleet`](#fleet); the home core unless the home moved away from it). A task runs on another machine with a copy of it (see [Running a task on another machine](#running-a-task-on-another-machine)).

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/projects` | | `[Project]` |
| `POST` | `/v0/projects` | `{ title, directory? }` | Project |
| `GET` | `/v0/projects/{slug}` | | Project + `content` |
| `PATCH` | `/v0/projects/{slug}` | any of `{ title, muted, archived, directory }` | Project. A new title renames the project and moves its tasks to the new slug. |
| `DELETE` | `/v0/projects/{slug}` | | `{ deleted }`. Also deletes the project's tasks, with their plans and attachments. |

## Plans

`{ slug, title, task, created }`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/plans` | | `[Plan]` |
| `GET` | `/v0/plans/{slug}` | | Plan + `content` (an empty placeholder if the plan hasn't been written yet) |
| `PUT` | `/v0/plans/{slug}` | `{ content, title?, task? }` | Plan |
| `DELETE` | `/v0/plans/{slug}` | | `{ deleted }` |

## Daily notes and weekly updates

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/daily` | | `[{ date, preview, summary }]`, newest first |
| `GET` | `/v0/daily/{YYYY-MM-DD}` | | `{ date, content, tasks }`: tasks due that day (incl. recurring) and undated tasks completed that day |
| `PUT` | `/v0/daily/{date}` | `{ content }` | `{ date }`; blank content deletes the note |
| `PUT` | `/v0/daily/{date}/summary` | `{ content, summary }` | `{ saved }`; only saved if the note still has `content` |
| `GET` | `/v0/weekly` | | `[{ weekId, generated, preview, summary }]`, newest first |
| `GET` | `/v0/weekly/{YYYY-Www}` | | `{ weekId, generated, content }` |
| `PUT` | `/v0/weekly/{week}` | `{ content, generated? }` | `{ weekId, generated }`; `generated: true` stamps it as freshly generated |
| `PUT` | `/v0/weekly/{week}/summary` | `{ content, summary }` | `{ saved }` |

## Events

`/v0/events` also carries `{ "type": "workspaceChanged", "areas": ["tasks", "plans"] }` when workspace files change, from the core or from outside (an editor, iCloud). Changes are coalesced over 150 ms.

# MCP (for coding agents)

The core is also the TUI-DO MCP server: 22 tools (`start_task`, `update_plan`, `complete_task`, `list_tasks`, `get_task`, `create_task`, `update_task`, `delete_task`, projects, plans, daily notes, weekly updates) with the same names, descriptions and parameters as the original Node server, so existing agent instructions keep working. Results are JSON text; business errors (unknown task, `PROJECT_MISMATCH`, `SUMMARY_REQUIRED`) are normal results with an `error` or `warning` field.

Two ways in:

- **stdio** — register `tui-do-core mcp` as the server command (e.g. in `~/.claude.json`: `{"command": "/Users/me/.tui-do/bin/tui-do-core", "args": ["mcp"]}`). It forwards every call to the running core. When Claude Code launches it, it also passes along which Claude session that is, so `start_task` links the task to the session.
- **HTTP** — `/mcp` (MCP streamable HTTP), with the core's token as `Authorization: Bearer <token>`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `POST` | `/v0/mcp/call` | `{ name, arguments, session? }` | `{ result }` or `{ error }` (what `tui-do-core mcp` uses) |
| `GET` | `/v0/tasks/{slug}/claude-session` | | `{ sessionId, cwd, linkedAt }` or 404 |
| `PUT` | `/v0/tasks/{slug}/claude-session` | `{ sessionId, cwd }` (UUID, absolute path) | the stored link |

# Paired clients

A client on another device (a phone, say) gets its own token from the home core instead of the core's token, so it can be revoked on its own. A client token opens every route except the ones below and the home role's plumbing (`/v0/home`, `PUT /v0/fleet/home`, `/v0/fleet/adopt`, `/v0/replica`), which only the core's own token can use (also through `/v0/devices/{id}/…`). The exception is the app on a Mac the home moved away from: its client (`manager: true` in the list) can also use the routes below on the home itself, so the Mac keeps managing phones. The core stores a SHA-256 hash of each client token in `clients.json`, never the token.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/clients` | | `[{ id, name, createdAt, lastSeenAt }]` (`lastSeenAt` is updated at most every 10 minutes) |
| `POST` | `/v0/clients` | `{ name }` | `{ client, token }`. The token (`tdc_…`) is only returned here. |
| `DELETE` | `/v0/clients/{id}` | | 204. The token stops working, and the client's open WebSockets close. |

The TUI-DO Mac app pairs a phone with a QR code holding a link: `tuido://pair?address=<home core's Tailscale URL>&token=<client token>&name=<home core's name>`. Paired clients move with the home, so a client token keeps working after a move; the client finds the new address as described in [Finding the home](#finding-the-home).

# Coding agents

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/agents` | | `{ claude, codex }`: what the agents installed on this machine offer, `null` for one that isn't installed. `claude`: `{ models: [{ value, label, description, effortLevels, supportsAutoMode }], permissionModes }` (the models `/model` lists, and the `--permission-mode` choices). `codex`: `{ models: [{ slug, label, efforts }] }`. Cached for 30 minutes. Ask a fleet device through `/v0/devices/{id}/agents`. |
| `POST` | `/v0/tasks/{ref}/agent` | `{ device?, agent?, model?, effort?, permissionMode?, yolo?, includeUncommitted?, worktree?, worktreePath?, cols?, rows? }` | `{ session, device, path, started }` |

`POST /v0/tasks/{ref}/agent` starts a coding agent on a task the way the Mac app does, so a client only picks the options:

- `device`: the machine to run on: a fleet device or the home core's own id (absent: the code machine). On the code machine the task works in the project's folder; on any other machine the task's code is handed over first, as with [`/handoff`](#running-a-task-on-another-machine) (`includeUncommitted` takes uncommitted changes along), and the task's `device` is set.
- `agent`: `claude` (default) or `codex`. `model` is Claude's `--model` value (`default` adds no flag) or a Codex model slug; `effort` is an effort level from `/v0/agents`; `permissionMode` is Claude's; `yolo` skips permission prompts (and Codex's sandbox).
- `worktree` (Claude only): a name for a new git worktree (`--worktree`). `worktreePath`: a worktree that already exists, which Claude starts in instead. Cores before 0.3.3 ignore both.
- The session is the task's slug, a login shell in the project's folder (or the task's folder on the machine) with `agent` set. Once the shell prints its first output (or after 5 s), the core types the agent's command with the prompt `Start working on task <id>`.
- If the task's session is already running there, it's returned as it is with `started: false`.

# Fleet

A home core can know other machines' cores (devices) and forward requests to them, so a client only needs the home core's address and token. Device tokens stay on the home core (in `fleet.json`, readable only by its user) and are never returned to clients. Devices reach each other over Tailscale: turn on `remoteAccess` on each device (`PATCH /v0/device`).

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/fleet` | | `{ generation, home: { id, name, platform, version }, codeDevice, devices: [{ id, name, address, platform, addedAt, online, version }] }` (reachability is checked live). `codeDevice` is the machine the projects' folders are on. |
| `POST` | `/v0/fleet/devices` | `{ address, token }` (e.g. `http://100.79.154.110:47821` and that core's token) | the device; it's asked who it is with the token first, so a wrong token or address is rejected. Adding a known device updates it. |
| `DELETE` | `/v0/fleet/devices/{id}` | | 204. The code machine can't be removed (`409`). |
| `GET` | `/v0/fleet/sessions` | | sessions on this core and every reachable device, each with a `device` id |
| any | `/v0/devices/{id}/…` | | forwarded to that device's `/v0/…` with its token, including WebSockets (`/v0/devices/{id}/sessions/{sid}/attach`, `/v0/devices/{id}/events`). `{id}` may also be the home core's own id. `502` if the device isn't reachable. |

## Installing on another machine

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `POST` | `/v0/fleet/install` | `{ host, port?, identityFile?, version?, instructions? }` | the new device, once it's installed and reachable (takes about a minute); `502` with the reason if it fails |

The home core connects with `ssh` (key-based, no prompts) to `host` (an alias from `~/.ssh/config`, `host` or `user@host`), and on that machine: downloads the GitHub release matching the home core's version for its platform (macOS arm64, Linux x86_64 or arm64), checks its SHA-256, installs it to `~/.tui-do/bin`, runs it as a systemd user service (Linux) or launchd agent (macOS), turns on remote access, and reports its Tailscale address and token, which the home core uses to add it to the fleet. Requires `curl`, `tar` and Tailscale on the target; on Linux, `loginctl enable-linger` keeps it running while you're logged out. Progress is sent on `/v0/events` as `{ "type": "installProgress", "host", "step" }`.

## Updating

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/update` | | `{ current, latest, canUpdate, rolledBack }`: this core's version, the newest GitHub release as last checked (the core checks when it starts and again when asked after 15 minutes, in the background; `null` until it knows), whether it can update itself, and `{ version, at }` if the last update didn't come up and the core went back (otherwise `null`) |
| `POST` | `/v0/update` | `{ version? }` (default: the latest) | `{ version }`, then the core restarts |
| `POST` | `/v0/fleet/devices/{id}/update` | `{ version?, instructions? }` | the device, once it's back on the new version |

A core updates itself by downloading the release for its platform into `~/.tui-do/versions/<version>/`, checking its SHA-256 and that it runs, pointing `~/.tui-do/bin/tui-do-core` at it, then exiting; its service (the launchd agent or systemd user service both installs set up) starts the new one. A core that isn't running as that service refuses (`400`). Sessions keep running through the restart, and clients that were mirroring one should fetch a fresh snapshot when they reconnect, for the output they missed. If the new version doesn't report itself healthy on `/v0/health` for 10 seconds within 45, the machine switches back to the previous version and restarts it. Cores before 0.3 end their sessions when they update, including the update to 0.3.

On the home core, `POST /v0/fleet/devices/{id}/update` has the device update itself and waits until it's back on the new version (up to 90 seconds), then sets up its coding agents again when `instructions` is given. A device whose core is too old to update itself (before 0.1.6) is reinstalled over the ssh host it was installed through, as with `POST /v0/fleet/install`.

## Fleet events and runners

The home core relays every device's events on its own `/v0/events`:

```json
{ "type": "deviceEvent", "device": "<id>", "event": { "type": "output", "id": "my-task", "data": "…" } }
{ "type": "deviceStatus", "device": "<id>", "online": false }
```

`event` is exactly what that device sends on its own stream (`sessions`, `sessionCreated`, `agentState`, …). A device sends the home only each session's first `output`, which tells it the shell is ready; to see a device's session, attach to it (`/v0/devices/{id}/sessions/{sid}/attach`). Without `?output=1`, even that first `output` is left out. Clients load the current state with `GET /v0/fleet` and `GET /v0/fleet/sessions`, then apply events. To type into a device's session over the same socket, add its id: `{ "type": "input", "device": "<id>", "id": "my-task", "data": "ls\n" }` (also for `resize`); the home core passes it on in order.

When a device is added, and whenever its events come back online, the home core turns on its own remote access and gives the device a home link (`PUT /v0/fleet/home { address, token, id, name, generation, clientToken? }` on the device): the home core's Tailscale address and a token that only opens the home core's MCP endpoints (`/mcp`, `/v0/mcp/call`). A core with a home link (a runner) forwards MCP tool calls there, so `tui-do-core mcp` works the same on every machine. A device refuses a link with an older `generation` than the one it has (`409`); a link without `clientToken` keeps the one the device has.

## The home role

One core is the home: it writes the workspace and holds the fleet (`fleet.json`), paired clients, task ↔ Claude session links and handoff records. The others are runners: they have a home link and a read-only copy of the workspace. The role can move to any fleet device.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/home` | | `{ self, id, name, address?, generation, clientToken? }`: where the home is (`self` on the home itself). `clientToken` is set on a Mac the home moved away from: a [paired-client](#paired-clients) token at the home, for the app on that Mac. |
| `POST` | `/v0/fleet/move` | `{ device }` | `{ home: { id, name, address }, generation }`, once the device is the home (on the home core) |

A move goes like this:

1. The device's copy of the workspace is brought up to date, while the workspace is still in use. A home whose iCloud folder hasn't downloaded every file refuses to move.
2. The home stops taking writes (`503`, MCP calls included), waits up to 30 seconds for writes under way to finish, and brings the copy up to date again, checking that nothing is left to send.
3. The device takes the fleet (minus itself, plus the old home), paired clients, session links and handoff records (`POST /v0/fleet/adopt`).
4. The device serves its copy read-write from then on, and links every device to itself as it reaches them. That includes devices that were offline during the move, once they're back.
5. The old home becomes a runner, with a read-only copy and a link to the new home. Its folder's contents are kept in `~/.tui-do/core/backups/` first (the newest two are kept). If the old home is a Mac, its link carries a client token for the app there. A Mac that never received it (the answer to the move got lost, say) gets a new one when the new home next links it.

`generation` goes up by one with each move. The projects' folders stay where they are: the first move away from a home makes that machine the code machine (`codeDevice`), and handoffs start from there and bring branches back there, wherever the home is. If the move fails before the device takes over, the old home takes writes again and stays the home. If it can't tell (the device stopped answering mid-move), it keeps refusing writes. A core that still holds a fleet gives the role away whenever a newer home links it, so the move finishes as soon as the new home reaches the old one. If the device never took over, restarting the old home's core ends the pause. Taking over twice from the same move is harmless. A device whose core is too old answers `404` to `adopt`; update it first.

Every core sends `{ "type": "homeChanged", "home": "<id>", "generation": 3 }` on its own `/v0/events` when it takes the home, gives it away, or is linked to another home.

### Finding the home

A client that only knew the old home's address, a phone say, asks any core it knows for `GET /v0/health` (no token needed) and connects to `home.address` (the one with the highest `generation`) with its client token. On the home itself, `home` has no `address`. The Mac app asks its own core for `GET /v0/home` and connects to the home with `clientToken` when the home is elsewhere.

### Workspace copies

The home keeps a copy of the workspace on every device: when a device comes online, and about 3 seconds after the workspace changes. Only files that differ (by size and modification time) are sent, and files gone from the home are deleted from the copy. Nothing is deleted while iCloud hasn't downloaded some of the home's files, or if the home has no tasks. The copy goes to `~/.tui-do/workspace`, or to a former home's own folder, which is marked as a copy when the home moves away from it. That way a Mac's iCloud folder stays current while the home is elsewhere, and stops being a copy when the Mac takes the home back. A device that serves a workspace folder of its own refuses copies (`409`), so joining a fleet never overwrites one. Dotfiles aren't copied. A core only takes copies from its own home at its current generation.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/replica` | | `{ files: { "<path>": [size, mtimeMs] }, complete }`: this core's copy (paths like `tasks/my-task.md`) |
| `POST` | `/v0/replica?home=&generation=` | a batch: a JSON line `{ files: [{ path, size, mtimeMs }], delete: [path] }`, then the files' bytes in that order (deletions are applied first) | `{ changed }`. `409` on the home, from a home that isn't this core's, or into a folder that isn't a copy. |

## Running a task on another machine

Starting a task on a fleet device takes the project's code along; completing the task brings the work back as a branch. Only git projects can move. The code comes from the code machine (`codeDevice`); when that's not the home core, the home core asks it for the snapshot and has it land the returned branch.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/projects/{slug}/git` | | `{ isRepo, uncommitted }`: whether the project folder is a git repo and how many files have uncommitted changes |
| `POST` | `/v0/tasks/{slug}/handoff` | `{ device, includeUncommitted? }` | `{ device, path, branch }`: the task's working folder on that device (the home core's own id works too). On the code machine itself that's the project's folder, with an empty `branch`. Otherwise the code machine snapshots the project's repo (unpushed commits always; uncommitted and untracked changes only with `includeUncommitted`; the checkout, index and branch are left alone), sends only the commits the device lacks as a `git bundle`, and the device checks it out in `~/.tui-do/work/<project>/<task>` on branch `tui-do/<task id>`, with uncommitted work still uncommitted. If the task already has a folder there, it's kept. |

Completing the task (in the app, through the API or from an MCP tool) makes the device commit what's left and send the branch back; the code machine fetches it into the project as `tui-do/<task id>` (your checkout is never touched). Uncommitted changes that came along and weren't touched there are left out, since they're still in your checkout. The device's folder, branch and the task's refs are removed; its cache of the project keeps only the committed history (`refs/tui-do/known`), so the next handoff sends just what's new. Progress goes out on `/v0/events` as `{ "type": "handoff", "task", "status": "starting" | "started" | "returned" | "failed", "branch", "message" }`.

Device-side routes used by the home core: `GET /v0/handoff/{project}/commits`, `POST /v0/handoff/{project}/{task}` (bundle body), `POST /v0/handoff/{project}/{task}/collect`; on the code machine, `POST /v0/handoff/{project}/{task}/snapshot` (`{ directory, known, includeUncommitted }`; the bundle as the body, the snapshot in the `x-tui-do-snapshot` header), `POST /v0/handoff/{project}/{task}/land?repo=&branch=` (bundle body) and `GET /v0/git/status?dir=`.

## Coding agents on a machine

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `PUT` | `/v0/agents/setup` | `{ instructions }` | `{ claude, codex }`: which agents were set up |

Registers `tui-do-core mcp` with the coding agents installed on that machine (`claude mcp add --scope user`, `codex mcp add`), writes `instructions` (a markdown section starting with `## TUI-DO Task Workflow (MCP Server)`) into `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` (replacing an earlier version), and allows `mcp__tui-do__*` in Claude Code's settings. `POST /v0/fleet/install` does this on the new machine when its body includes `instructions`.
