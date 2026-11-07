# API v0

Base URL: the contents of `~/.tui-do/core/endpoint` (default `http://127.0.0.1:47821`).

Every route except `/v0/health` needs the token from `~/.tui-do/core/token`, sent as `Authorization: Bearer <token>` or as a `?token=` query parameter (browsers can't set headers on WebSockets). Errors are JSON: `{ "error": "message" }`.

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
| `GET` | `/v0/health` | | `{ name, version, apiVersion }` (no token needed) |
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

### `GET /v0/sessions/{id}/attach`

One session. The server first sends

```json
{ "type": "snapshot", "data": "<VT>", "session": { ... } }
```

then that session's events (see below) in order, with nothing lost or repeated between the snapshot and the first event. A `snapshot` message always means *reset, then write*: it's sent again if the client fell too far behind to receive every event. The socket closes after `sessionRemoved`.

Client → server:

```json
{ "type": "input", "data": "ls\n" }
{ "type": "resize", "cols": 120, "rows": 40 }
```

### `GET /v0/events` and `GET /v0/events?output=1`

Every session. The server first sends `{ "type": "sessions", "sessions": [Session] }`, then events. `output` events are only included with `?output=1`. If the client fell behind, the server sends `{ "type": "resync", "sessions": [Session] }`; clients mirroring output should fetch fresh snapshots.

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
```

`output` data is always valid UTF-8; a character split across PTY reads is held back until it's complete. `sessionQuiet` fires when a session has produced no output for 2 seconds.

## Notes for clients

- The core doesn't answer terminal queries (device attributes, cursor position reports); the client's terminal emulator does, by sending its replies as input. If several clients are attached, each answers.
- Resizes apply to the PTY; the last client to resize wins.
- While a full-screen app uses the alternate screen, snapshots don't include the primary screen's scrollback.

# Workspace

The core serves one workspace: a folder of markdown files with YAML frontmatter (`tasks/`, `projects/`, `plans/`, `daily/`, `weekly/`, `attachments/`). Files are read and written exactly like the original TUI-DO app, so the folder can be shared with it (and synced with iCloud). Until a workspace is set, these routes return `409`.

Writes are atomic and serialized: task IDs are never handed out twice, and creating something whose name is taken picks a free slug (`my-task-2`) instead of overwriting. Errors: `404` not found, `400` invalid input.

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
| `POST` | `/v0/tasks/{slug}/attachments` | `{ filePath }` or `{ fileName, dataBase64 }` (stored under `attachments/<slug>/`) | Task + `description` |
| `POST` | `/v0/tasks/{slug}/attachments/remove` | `{ filePath }` | Task + `description` |

### Agent workflow

These take a short ID or slug and answer like the TUI-DO MCP tools (business errors are `200` with an `error` or `warning` field).

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `POST` | `/v0/tasks/{ref}/start` | `{ expectedProject? }` | `{ status: "started", task, plan: { slug, content } }`, or a `PROJECT_MISMATCH` warning with alternatives. Reserves a plan slug. |
| `PUT` | `/v0/tasks/{ref}/plan` | `{ content }` | `{ status: "updated", plan: { slug, task } }` |
| `POST` | `/v0/tasks/{ref}/complete` | `{ summary }` | `{ status: "completed" \| "recurring_advanced", task }`; the summary is stored in the plan as `## Completion Summary`. Without a summary: `SUMMARY_REQUIRED`. |

## Projects

`{ slug, title, muted, created, directory, ssh? }`, where `ssh` is `{ host, user?, port?, identityFile? }`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/v0/projects` | | `[Project]` |
| `POST` | `/v0/projects` | `{ title, directory?, ssh? }` | Project |
| `GET` | `/v0/projects/{slug}` | | Project + `content` |
| `PATCH` | `/v0/projects/{slug}` | any of `{ title, muted, directory, ssh }` (`ssh: null` removes it) | Project. A new title renames the project and moves its tasks to the new slug. |
| `DELETE` | `/v0/projects/{slug}` | | `{ deleted }` |

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
- **HTTP** — `/mcp` (MCP streamable HTTP), with the core's token as `Authorization: Bearer <token>`. Useful from a remote machine through an SSH reverse tunnel.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `POST` | `/v0/mcp/call` | `{ name, arguments, session? }` | `{ result }` or `{ error }` (what `tui-do-core mcp` uses) |
| `POST` | `/v0/mcp/tokens` | | `{ token }`: a token that only opens `/mcp`, for a remote machine (kept until revoked or the core restarts) |
| `DELETE` | `/v0/mcp/tokens/{token}` | | 204 |
| `GET` | `/v0/tasks/{slug}/claude-session` | | `{ sessionId, cwd, linkedAt }` or 404 |
| `PUT` | `/v0/tasks/{slug}/claude-session` | `{ sessionId, cwd }` (UUID, absolute path) | the stored link |
