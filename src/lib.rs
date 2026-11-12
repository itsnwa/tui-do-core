//! TUI-DO core: a background service that owns terminal sessions and the
//! task workspace, and serves them to clients over HTTP + WebSocket.

pub mod activity;
pub mod agent_models;
pub mod agents;
pub mod api;
pub mod apns;
pub mod claude_status;
pub mod clients;
pub mod config;
pub mod dictation;
pub mod fleet;
pub mod format;
pub mod handoff;
pub mod holder;
pub mod hooks;
pub mod install;
pub mod links;
pub mod mcp;
pub mod net;
pub mod notify;
pub mod pace;
pub mod relay;
pub mod replica;
pub mod session;
pub mod status;
pub mod update;
pub mod uploads;
pub mod usage;
mod utf8;
pub mod workspace;
