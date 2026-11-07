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
