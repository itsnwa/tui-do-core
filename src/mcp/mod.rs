//! The TUI-DO MCP server, for coding agents (Claude Code, Codex).
//!
//! Tools always run in the core daemon. Agents reach them two ways:
//! - `/mcp` on the daemon (streamable HTTP, token required), e.g. from a
//!   remote ssh session through a reverse tunnel;
//! - `tui-do-core mcp`, a stdio server the agent launches. It forwards each
//!   call to the daemon, along with the Claude session that launched it, so
//!   `start_task` can link the task to that session.

pub mod tools;

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigStore, HomeLink};
use crate::links::{SessionLink, TaskSessions};
use crate::workspace::shared::SharedWorkspace;

/// A tool call forwarded from `tui-do-core mcp` to the daemon (`POST /v0/mcp/call`).
#[derive(Debug, Serialize, Deserialize)]
pub struct ForwardedCall {
    pub name: String,
    #[serde(default)]
    pub arguments: JsonObject,
    #[serde(default)]
    pub session: Option<SessionLink>,
}

/// The daemon's answer to a forwarded call: a result, or an error message.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallOutcome {
    Result(serde_json::Value),
    Error(String),
}

/// Send a call to another core's `POST /v0/mcp/call`.
async fn forward_call(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    call: &ForwardedCall,
) -> CallOutcome {
    let sent = client
        .post(format!("{endpoint}/v0/mcp/call"))
        .bearer_auth(token)
        .json(call)
        .send()
        .await;
    match sent {
        Ok(res) if res.status().is_success() => res
            .json::<CallOutcome>()
            .await
            .unwrap_or_else(|e| CallOutcome::Error(format!("bad answer from tui-do-core: {e}"))),
        Ok(res) => CallOutcome::Error(format!("tui-do-core answered {}", res.status())),
        Err(e) => CallOutcome::Error(format!("tui-do-core isn't reachable at {endpoint}: {e}")),
    }
}

/// Run a call in the daemon: against its workspace, off the async workers,
/// or, on a runner (no workspace), on the home core it was linked to.
pub async fn run_call(
    workspace: Arc<SharedWorkspace>,
    sessions: Arc<TaskSessions>,
    home: Option<HomeLink>,
    call: ForwardedCall,
) -> CallOutcome {
    let Some(ws) = workspace.current() else {
        if let Some(home) = home {
            return forward_call(&reqwest::Client::new(), &home.address, &home.token, &call).await;
        }
        return CallOutcome::Error(
            "Workspace not configured. Please select a workspace folder first.".into(),
        );
    };
    let outcome = tokio::task::spawn_blocking(move || {
        tools::call(
            &ws,
            &sessions,
            &call.name,
            call.arguments,
            call.session.as_ref(),
        )
    })
    .await;
    match outcome {
        Ok(Ok(value)) => CallOutcome::Result(value),
        Ok(Err(message)) => CallOutcome::Error(message),
        Err(err) => CallOutcome::Error(err.to_string()),
    }
}

#[derive(Clone)]
enum Backend {
    /// Inside the daemon.
    Local {
        workspace: Arc<SharedWorkspace>,
        sessions: Arc<TaskSessions>,
        config: Arc<ConfigStore>,
    },
    /// In `tui-do-core mcp`: forward to the daemon over HTTP.
    Daemon {
        endpoint: String,
        token: String,
        client: reqwest::Client,
        session: Option<SessionLink>,
    },
}

#[derive(Clone)]
pub struct TuiDoMcp {
    backend: Backend,
}

impl TuiDoMcp {
    pub fn local(
        workspace: Arc<SharedWorkspace>,
        sessions: Arc<TaskSessions>,
        config: Arc<ConfigStore>,
    ) -> Self {
        Self {
            backend: Backend::Local {
                workspace,
                sessions,
                config,
            },
        }
    }

    pub fn forwarding(endpoint: String, token: String, session: Option<SessionLink>) -> Self {
        Self {
            backend: Backend::Daemon {
                endpoint,
                token,
                client: reqwest::Client::new(),
                session,
            },
        }
    }

    async fn call(&self, name: String, arguments: JsonObject) -> CallOutcome {
        match &self.backend {
            Backend::Local {
                workspace,
                sessions,
                config,
            } => {
                let call = ForwardedCall {
                    name,
                    arguments,
                    session: None,
                };
                run_call(
                    Arc::clone(workspace),
                    Arc::clone(sessions),
                    config.get().home,
                    call,
                )
                .await
            }
            Backend::Daemon {
                endpoint,
                token,
                client,
                session,
            } => {
                let call = ForwardedCall {
                    name,
                    arguments,
                    session: session.clone(),
                };
                forward_call(client, endpoint, token, &call).await
            }
        }
    }
}

impl ServerHandler for TuiDoMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tui-do", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools::definitions(),
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let result = match self.call(name, request.arguments.unwrap_or_default()).await {
            CallOutcome::Result(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            CallOutcome::Error(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        };
        Ok(result.into())
    }
}
