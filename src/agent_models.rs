//! What the coding agents installed on this machine offer when starting a
//! task: Claude Code's models (with their effort levels) and permission modes,
//! and Codex's models. Asked from the CLIs themselves, so a client's choices
//! never go stale, and cached for a while.
//!
//! - Claude: the stream-json `list_models` control request (the list `/model`
//!   shows; no prompt is sent, so it costs nothing), and the
//!   `--permission-mode` choices in `claude --help`.
//! - Codex: `codex debug models`.

use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
const CACHE_FOR: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeModel {
    /// Passed to `--model`; `default` means no flag.
    pub value: String,
    /// The versioned name `/model` shows, e.g. "Opus 5.5".
    pub label: String,
    pub description: String,
    /// Empty when the model takes no effort level.
    pub effort_levels: Vec<String>,
    pub supports_auto_mode: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeOptions {
    pub models: Vec<ClaudeModel>,
    pub permission_modes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexModel {
    pub slug: String,
    pub label: String,
    pub efforts: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOptions {
    pub models: Vec<CodexModel>,
}

/// `null` for an agent that isn't installed (or couldn't be asked).
#[derive(Clone, Debug, Serialize)]
pub struct AgentOptions {
    pub claude: Option<ClaudeOptions>,
    pub codex: Option<CodexOptions>,
}

static CACHE: Mutex<Option<(Instant, AgentOptions)>> = Mutex::new(None);

/// The agents' options, from the cache when it's fresh.
pub async fn options() -> AgentOptions {
    if let Some((at, cached)) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
        && at.elapsed() < CACHE_FOR
    {
        return cached.clone();
    }
    let (claude, codex) = tokio::join!(claude_options(), codex_options());
    let options = AgentOptions {
        claude: claude
            .inspect_err(|err| tracing::debug!(%err, "no Claude Code options"))
            .ok(),
        codex: codex
            .inspect_err(|err| tracing::debug!(%err, "no Codex options"))
            .ok(),
    };
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), options.clone()));
    options
}

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME").map_or_else(|| "/".into(), Into::into)
}

async fn claude_options() -> anyhow::Result<ClaudeOptions> {
    let (models, permission_modes) = tokio::join!(claude_models(), claude_permission_modes());
    Ok(ClaudeOptions {
        models: models?,
        permission_modes: permission_modes.unwrap_or_default(),
    })
}

/// Start Claude in stream-json mode, send `list_models` and read the answer.
async fn claude_models() -> anyhow::Result<Vec<ClaudeModel>> {
    let mut child = Command::new("claude")
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
        ])
        .current_dir(home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting claude")?;
    let mut stdin = child.stdin.take().context("claude's stdin")?;
    let stdout = child.stdout.take().context("claude's stdout")?;
    for request in [
        json!({ "type": "control_request", "request_id": "init", "request": { "subtype": "initialize" } }),
        json!({ "type": "control_request", "request_id": "list-models", "request": { "subtype": "list_models" } }),
    ] {
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
    }
    stdin.flush().await?;

    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let response = &msg["response"];
            if msg["type"] != "control_response" || response["request_id"] != "list-models" {
                continue;
            }
            if response["subtype"] != "success" {
                bail!("list_models failed: {}", response["error"]);
            }
            let models: Vec<_> = response["response"]["models"]
                .as_array()
                .map(|list| list.iter().filter_map(parse_claude_model).collect())
                .unwrap_or_default();
            if models.is_empty() {
                bail!("Claude returned no models");
            }
            return Ok(models);
        }
        bail!("Claude exited before listing its models")
    };
    let models = tokio::time::timeout(QUERY_TIMEOUT, read)
        .await
        .context("timed out waiting for Claude's models")?;
    let _ = child.kill().await;
    models
}

fn parse_claude_model(raw: &Value) -> Option<ClaudeModel> {
    let value = raw["value"].as_str().filter(|v| !v.is_empty())?;
    let display_name = raw["displayName"].as_str().unwrap_or(value);
    let full = raw["description"].as_str().unwrap_or("");
    // Some versions put the versioned name in the description ("Opus 5.5 ·
    // Best for…") and only an alias in displayName ("Opus")
    let (versioned, rest) = full
        .split_once(" · ")
        .map_or(("", full), |(name, rest)| (name.trim(), rest));
    let label = if value != "default" && !versioned.is_empty() {
        versioned
    } else {
        display_name
    };
    let effort_levels = if raw["supportsEffort"] == false {
        Vec::new()
    } else {
        raw["supportedEffortLevels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(ClaudeModel {
        value: value.to_string(),
        label: label.to_string(),
        description: rest.to_string(),
        effort_levels,
        supports_auto_mode: raw["supportsAutoMode"] == true,
    })
}

async fn claude_permission_modes() -> anyhow::Result<Vec<String>> {
    let out = tokio::time::timeout(
        QUERY_TIMEOUT,
        Command::new("claude")
            .arg("--help")
            .current_dir(home())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("timed out")??;
    Ok(parse_permission_modes(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

fn parse_permission_modes(help: &str) -> Vec<String> {
    let Ok(choices) =
        regex::Regex::new(r#"--permission-mode\s+<mode>[\s\S]*?\(choices:([\s\S]*?)\)"#)
    else {
        return Vec::new();
    };
    let Some(list) = choices.captures(help).and_then(|c| c.get(1)) else {
        return Vec::new();
    };
    list.as_str()
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

async fn codex_options() -> anyhow::Result<CodexOptions> {
    let out = tokio::time::timeout(
        QUERY_TIMEOUT,
        Command::new("codex")
            .args(["debug", "models"])
            .current_dir(home())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("timed out")?
    .context("running codex")?;
    if !out.status.success() {
        bail!("codex debug models failed");
    }
    let catalog: Value = serde_json::from_slice(&out.stdout).context("codex's model catalog")?;
    Ok(CodexOptions {
        models: parse_codex_models(&catalog),
    })
}

fn parse_codex_models(catalog: &Value) -> Vec<CodexModel> {
    let Some(models) = catalog["models"].as_array() else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|m| m["visibility"] != "hide")
        .filter_map(|m| {
            let slug = m["slug"].as_str().filter(|s| !s.is_empty())?;
            let label = m["display_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(slug);
            let efforts = m["supported_reasoning_levels"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|e| e["effort"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Some(CodexModel {
                slug: slug.to_string(),
                label: label.to_string(),
                efforts,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_model_should_take_the_versioned_name_from_the_description() {
        let model = parse_claude_model(&json!({
            "value": "opus",
            "displayName": "Opus",
            "description": "Opus 5.5 · Most capable",
            "supportedEffortLevels": ["low", "high"],
        }))
        .expect("parses");
        assert_eq!(model.label, "Opus 5.5");
        assert_eq!(model.description, "Most capable");
        assert_eq!(model.effort_levels, ["low", "high"]);
    }

    #[test]
    fn permission_modes_should_come_from_the_help_choices() {
        let help = r#"  --permission-mode <mode>  Permission mode (choices: "acceptEdits", "plan", "default")"#;
        assert_eq!(
            parse_permission_modes(help),
            ["acceptEdits", "plan", "default"]
        );
    }

    #[test]
    fn hidden_codex_models_should_be_left_out() {
        let catalog = json!({ "models": [
            { "slug": "gpt-6", "display_name": "GPT-6", "supported_reasoning_levels": [{ "effort": "high" }] },
            { "slug": "old", "visibility": "hide" },
        ]});
        let models = parse_codex_models(&catalog);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].efforts, ["high"]);
    }
}
