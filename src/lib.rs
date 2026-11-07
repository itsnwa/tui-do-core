//! TUI-DO core: a background service that owns terminal sessions and the
//! task workspace, and serves them to clients over HTTP + WebSocket.

pub mod agents;
pub mod api;
pub mod config;
pub mod fleet;
pub mod format;
pub mod handoff;
pub mod hooks;
pub mod install;
pub mod links;
pub mod mcp;
pub mod net;
pub mod relay;
pub mod session;
mod utf8;
pub mod workspace;
