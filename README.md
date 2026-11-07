# tui-do-core

A background service that owns TUI-DO terminal sessions and serves them to clients — the TUI-DO Mac app today, an iOS app and web clients next — over an HTTP + WebSocket API.

Sessions run inside the core, not inside a client, so they keep running when a client quits and can be watched or driven from any device. Each session's output is mirrored into a headless [libghostty-vt](https://ghostty.org) terminal, so a client that attaches gets the current screen (with scrollback, styles and modes) as a VT snapshot, then live output.

> Status: early. Terminal sessions, the task workspace and the MCP server for agents work. Multi-machine fleets and code handoff between machines are planned.

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

On macOS, `scripts/install-dev.sh` builds a release binary, installs it to `~/.tui-do/bin/tui-do-core` and runs it as a launchd agent (`dev.tui-do.core`) that starts at login and restarts if it exits. Reinstalling restarts the core, which ends running sessions. Uninstall steps are at the top of the script.

| Setting | Default |
| --- | --- |
| `TUI_DO_CORE_ADDR` | `127.0.0.1:47821` |
| `TUI_DO_CORE_DIR` | `~/.tui-do/core` |
| `TUI_DO_WORKSPACE` | the workspace set through the API (overrides it, for testing) |
| `RUST_LOG` | `tui_do_core=info` |

The core writes `token` (bearer token, `0600`), `endpoint` (base URL) and `config.json` (device id and name, workspace, remote access) to its directory. Clients on the same machine read the token and endpoint from there. The core listens on loopback; turning on remote access (`PATCH /v0/device`) also makes it listen on the machine's Tailscale address.

## API

See [docs/api.md](docs/api.md). The API is versioned by path (`/v0`); v0 may still change.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
