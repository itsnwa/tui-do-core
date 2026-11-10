# tui-do-core

A background service that owns TUI-DO's tasks and terminal sessions and serves them to clients — the TUI-DO Mac app, the [iOS app](https://github.com/itsnwa/tui-do-ios), and web clients next — over an HTTP + WebSocket API. It also serves the TUI-DO tools to coding agents over MCP.

Sessions run inside the core, not inside a client, so they keep running when a client quits and can be watched or driven from any device. Each session's output is mirrored into a headless [libghostty-vt](https://ghostty.org) terminal, so a client that attaches gets the current screen (with scrollback, styles and modes) as a VT snapshot, then live output.

The core runs on every machine you work from. One of them, the home core, holds the task workspace; the others run tasks for it, reached over [Tailscale](https://tailscale.com).

> Status: early, and the API (`/v0`) may still change. Sessions, the task workspace, the MCP server, machines, code handoff, self-update, paired clients and phone notifications (APNs) work.

## Build

Needs Rust (edition 2024) and **Zig 0.15.2** on `PATH` — the `libghostty-vt` crate builds Ghostty's VT library with it.

```sh
brew install zig@0.15 && brew link --force zig@0.15   # macOS
cargo build --release
cargo test
```

## Run

```sh
./target/release/tui-do-core
```

On macOS, `scripts/install-dev.sh` builds a release binary, installs it to `~/.tui-do/bin/tui-do-core` and runs it as a launchd agent (`dev.tui-do.core`) that starts at login and restarts if it exits. Reinstalling restarts the core; running sessions keep going. Uninstall steps are at the top of the script. Once it's installed, the core can update itself to new releases.

| Setting | Default |
| --- | --- |
| `TUI_DO_CORE_ADDR` | `127.0.0.1:47821` |
| `TUI_DO_CORE_DIR` | `~/.tui-do/core` |
| `TUI_DO_WORKSPACE` | the workspace set through the API (overrides it, for testing) |
| `RUST_LOG` | `tui_do_core=info` |

The core writes `token` (bearer token, `0600`), `endpoint` (base URL) and `config.json` (device id and name, workspace, remote access) to its directory. Clients on the same machine read the token and endpoint from there. The core listens on loopback; turning on remote access (`PATCH /v0/device`) also makes it listen on the machine's Tailscale address.

## Machines

Other machines join through the home core, which needs `ssh` access to them (key-based) and Tailscale on both ends:

- **Adding a machine** installs the release for its platform (macOS arm64, Linux x86_64 or arm64) as a launchd agent or systemd user service, links it to the home core, and sets up Claude Code and Codex there with the TUI-DO tools.
- **Running a task there** sends the project's code along (a git bundle with only the commits the machine lacks, and optionally your uncommitted changes). The agent works in its own folder on branch `tui-do/<task id>`. Completing the task brings that branch back to your repo without touching your checkout, and cleans up on the machine.
- **Sessions on any machine** show up on the home core, so a client can follow and type into all of them through one connection.
- **Phones and other clients** pair with the home core and get their own token, which can be revoked on its own. They start agents on any machine (`POST /v0/tasks/{ref}/agent`) and follow sessions through the home core. Two clients on one session share its size: the one typing has it.
- **Updating:** each core downloads the new release, checks its SHA-256 and that it runs, switches to it and lets its service restart it. The home core can update every machine. Running sessions keep going, because each one's program is kept by a small holder process that outlives the core (`src/holder.rs`). If a new version doesn't come up healthy within 45 seconds, the machine goes back to the previous one.
- **Copies of the workspace:** every machine keeps a read-only copy of the tasks, kept up to date by the home core a few seconds after each change. Agents on any machine still write through the home.
- **Moving the home** to another machine (say, one that's always on) takes a second, since its copy is already there. That machine takes the machines list, paired phones and running handoffs along with it. The code stays where it is: projects' folders are on the machine the home first moved away from, handoffs start from there, and branches come back there. The Mac app follows the move.

## API

See [docs/api.md](docs/api.md): sessions, tasks and the rest of the workspace, events, paired clients, starting agents, machines, handoff and updating. The API is versioned by path (`/v0`); v0 may still change.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
