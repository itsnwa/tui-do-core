//! How much of their plans the coding agents on this machine have used:
//! Claude's and Codex's 5-hour and weekly limits, for clients to show.
//!
//! - Claude, two ways, the newer wins:
//!   - its status line. The sessions TUI-DO starts run `tui-do-core
//!     statusline` (see `hooks`), which Claude hands its limits after each
//!     response. It saves them here and runs the user's own status line.
//!   - the login Claude Code keeps (the Keychain on macOS, else
//!     `.credentials.json`), asking Anthropic. The token is never refreshed
//!     here, which would rotate Claude Code's own: once it has expired, only
//!     the status line tells.
//! - Codex: its ChatGPT login (`auth.json`), asking ChatGPT. Codex refreshes
//!   that login itself.
//!
//! Neither of the services asked is documented; whatever they answer that
//! isn't understood reads as no usage.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apns::quoted;
use crate::dictation::{curl, reason};
use crate::workspace::dates::now_millis;

/// Where the status line keeps Claude's limits, in the core's folder.
const STATUS_LINE_FILE: &str = "claude-usage.json";
/// How long an unchanged status line record goes before it's saved again,
/// to say it's still current.
const RESAVE_AFTER_MS: i64 = 60_000;
/// How long an answer is kept, and how long after a failure to ask again.
const CACHE_FOR: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// e.g. "5-hour", "weekly".
    pub label: String,
    /// 0–100.
    pub used_percent: f64,
    /// When it starts over, in milliseconds; none when it hasn't started.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Claude's status line, in a session TUI-DO started.
    StatusLine,
    /// The agent's login, asking its service.
    Account,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Usage {
    Ok {
        /// e.g. "max", "plus".
        plan: Option<String>,
        /// When it was so, in milliseconds.
        as_of: i64,
        source: Source,
        /// The machine that said so.
        #[serde(default)]
        machine: Option<String>,
        windows: Vec<Window>,
    },
    Unavailable {
        reason: String,
    },
}

impl Usage {
    fn unavailable(reason: impl Into<String>) -> Self {
        Usage::Unavailable {
            reason: reason.into(),
        }
    }

    fn as_of(&self) -> Option<i64> {
        match self {
            Usage::Ok { as_of, .. } => Some(*as_of),
            Usage::Unavailable { .. } => None,
        }
    }

    fn on_plan(mut self, known: Option<String>) -> Self {
        if let Usage::Ok { plan, .. } = &mut self
            && plan.is_none()
        {
            *plan = known;
        }
        self
    }

    pub fn from_machine(mut self, name: &str) -> Self {
        if let Usage::Ok { machine, .. } = &mut self {
            machine.get_or_insert_with(|| name.to_string());
        }
        self
    }

    /// Windows that have started over since read as unused.
    pub fn aged(mut self, now: i64) -> Self {
        if let Usage::Ok { windows, .. } = &mut self {
            for window in windows {
                if window.resets_at.is_some_and(|at| at <= now) {
                    window.used_percent = 0.0;
                    window.resets_at = None;
                }
            }
        }
        self
    }
}

/// The newer of two answers; an unavailable one only when both are.
pub fn freshest(a: Usage, b: Usage) -> Usage {
    match (a.as_of(), b.as_of()) {
        (Some(x), Some(y)) if y > x => b,
        (None, Some(_)) => b,
        _ => a,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub claude: Usage,
    pub codex: Usage,
}

/// This machine's usage.
pub async fn here(core_dir: &Path) -> Report {
    let (account, codex) = tokio::join!(
        cached(&CLAUDE, claude_account),
        cached(&CODEX, codex_account)
    );
    let claude = match status_line_record(core_dir) {
        Some(recorded) => freshest(account, recorded.on_plan(claude_plan())),
        None => account,
    };
    Report { claude, codex }
}

// ── Caching ────────────────────────────────────────────────────

type Cache = tokio::sync::Mutex<Option<(Instant, Duration, Usage)>>;

static CLAUDE: Cache = tokio::sync::Mutex::const_new(None);
static CODEX: Cache = tokio::sync::Mutex::const_new(None);
/// Claude's plan, as its login last said: also when the token had expired.
static CLAUDE_PLAN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn claude_plan() -> Option<String> {
    CLAUDE_PLAN.lock().ok()?.clone()
}

/// One question at a time: callers meanwhile wait for its answer.
async fn cached<F, Fut>(cache: &Cache, ask: F) -> Usage
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Usage>,
{
    let mut held = cache.lock().await;
    if let Some((at, keep, usage)) = held.as_ref()
        && at.elapsed() < *keep
    {
        return usage.clone();
    }
    let usage = ask().await;
    let keep = match usage {
        Usage::Ok { .. } => CACHE_FOR,
        Usage::Unavailable { .. } => RETRY_AFTER,
    };
    *held = Some((Instant::now(), keep, usage.clone()));
    usage
}

// ── Claude ─────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Claude Code's login, as it keeps it.
async fn claude_login() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = tokio::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        if let Ok(Ok(out)) = tokio::time::timeout(REQUEST_TIMEOUT, out).await
            && out.status.success()
        {
            return Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
    }
    let file = crate::claude_status::config_dir()?.join(".credentials.json");
    tokio::fs::read_to_string(file).await.ok()
}

async fn claude_account() -> Usage {
    let Some(login) = claude_login().await else {
        return Usage::unavailable("not signed in to claude code");
    };
    let oauth = serde_json::from_str::<Value>(&login)
        .map(|login| login["claudeAiOauth"].clone())
        .unwrap_or_default();
    let Some(token) = oauth["accessToken"].as_str() else {
        return Usage::unavailable("not signed in to claude code");
    };
    let plan = oauth["subscriptionType"].as_str().map(str::to_string);
    if let Ok(mut known) = CLAUDE_PLAN.lock() {
        known.clone_from(&plan);
    }
    if oauth["expiresAt"]
        .as_i64()
        .is_some_and(|at| at <= now_millis())
    {
        return Usage::unavailable("claude code's login needs refreshing");
    }
    let config = [
        format!(
            "url = {}",
            quoted("https://api.anthropic.com/api/oauth/usage")
        ),
        format!(
            "header = {}",
            quoted(&format!("authorization: Bearer {token}"))
        ),
        format!("header = {}", quoted("anthropic-beta: oauth-2025-04-20")),
        format!("header = {}", quoted("user-agent: tui-do")),
    ];
    match curl(&config, REQUEST_TIMEOUT).await {
        Ok((200, body)) => match claude_windows(&body) {
            Some(windows) => Usage::Ok {
                plan,
                as_of: now_millis(),
                source: Source::Account,
                machine: None,
                windows,
            },
            None => Usage::unavailable("anthropic's answer wasn't understood"),
        },
        Ok((401 | 403, body)) => {
            tracing::info!("claude usage: login refused: {}", reason(&body));
            Usage::unavailable("claude code's login isn't accepted for usage")
        }
        Ok((status, body)) => {
            tracing::warn!("claude usage: {status}: {}", reason(&body));
            Usage::unavailable(format!("anthropic answered {status}"))
        }
        Err(err) => {
            tracing::warn!("claude usage: {err:#}");
            Usage::unavailable("couldn't reach anthropic")
        }
    }
}

/// Anthropic's answer: `five_hour`, `seven_day`, … each with a
/// `utilization` and an RFC 3339 `resets_at`.
fn claude_windows(body: &str) -> Option<Vec<Window>> {
    let json: Value = serde_json::from_str(body).ok()?;
    let windows: Vec<Window> = [
        ("five_hour", "5-hour"),
        ("seven_day", "weekly"),
        ("seven_day_opus", "weekly opus"),
        ("seven_day_sonnet", "weekly sonnet"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let window = &json[key];
        Some(Window {
            label: label.to_string(),
            used_percent: window["utilization"].as_f64()?,
            resets_at: window["resets_at"].as_str().and_then(|at| {
                chrono::DateTime::parse_from_rfc3339(at)
                    .ok()
                    .map(|at| at.timestamp_millis())
            }),
        })
    })
    .collect();
    (!windows.is_empty()).then_some(windows)
}

/// The limits in what Claude hands its status line: `rate_limits.five_hour`
/// and `.seven_day`, each with a `used_percentage` and `resets_at` in
/// seconds. Only there for a subscription, after the session's first response.
fn status_line_windows(input: &Value) -> Option<Vec<Window>> {
    let limits = &input["rate_limits"];
    let windows: Vec<Window> = [("five_hour", "5-hour"), ("seven_day", "weekly")]
        .into_iter()
        .filter_map(|(key, label)| {
            let window = &limits[key];
            Some(Window {
                label: label.to_string(),
                used_percent: window["used_percentage"].as_f64()?,
                resets_at: window["resets_at"].as_i64().map(|secs| secs * 1000),
            })
        })
        .collect();
    (!windows.is_empty()).then_some(windows)
}

#[derive(Serialize, Deserialize)]
struct Recorded {
    at: i64,
    windows: Vec<Window>,
}

/// What the status line last saw.
fn status_line_record(core_dir: &Path) -> Option<Usage> {
    let text = std::fs::read_to_string(core_dir.join(STATUS_LINE_FILE)).ok()?;
    let recorded: Recorded = serde_json::from_str(&text).ok()?;
    Some(Usage::Ok {
        plan: None,
        as_of: recorded.at,
        source: Source::StatusLine,
        machine: None,
        windows: recorded.windows,
    })
}

/// Save the limits the status line saw, unless they're saved already.
fn record(core_dir: &Path, windows: Vec<Window>, now: i64) -> anyhow::Result<()> {
    let path = core_dir.join(STATUS_LINE_FILE);
    let saved = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Recorded>(&text).ok());
    if saved.is_some_and(|saved| saved.windows == windows && now - saved.at < RESAVE_AFTER_MS) {
        return Ok(());
    }
    // Sessions save at once: each writes its own file, then moves it in place
    let temp = core_dir.join(format!("{STATUS_LINE_FILE}.{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(&Recorded { at: now, windows })?)?;
    std::fs::rename(&temp, &path).context("saving claude's limits")
}

/// The status line the user set for Claude, which TUI-DO's takes the place
/// of: the project's local settings, else its shared ones, else the user's.
fn own_status_line(input: &Value) -> Option<String> {
    let project = input["workspace"]["project_dir"]
        .as_str()
        .or_else(|| input["cwd"].as_str())
        .map(PathBuf::from);
    let mut files = Vec::new();
    if let Some(project) = project {
        files.push(project.join(".claude").join("settings.local.json"));
        files.push(project.join(".claude").join("settings.json"));
    }
    files.extend(crate::claude_status::config_dir().map(|dir| dir.join("settings.json")));
    files.iter().find_map(|file| {
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
        let line = &settings["statusLine"];
        (line["type"].as_str().unwrap_or("command") == "command")
            .then(|| line["command"].as_str())
            .flatten()
            .filter(|command| !command.trim().is_empty())
            .map(str::to_string)
    })
}

/// `tui-do-core statusline`: Claude's status line in the sessions TUI-DO
/// starts, with what Claude hands it on stdin. Runs the user's own status
/// line, so theirs still shows, then saves Claude's limits.
///
/// It never fails: at worst the status line is empty.
pub fn run_status_line(core_dir: Option<&Path>) {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let json: Value = serde_json::from_str(&input).unwrap_or_default();
    if let Some(command) = own_status_line(&json) {
        let child = std::process::Command::new("sh")
            .args(["-c", &command])
            .stdin(Stdio::piped())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes());
            }
            let _ = child.wait();
        }
    }
    if let (Some(dir), Some(windows)) = (core_dir, status_line_windows(&json))
        && let Err(err) = record(dir, windows, now_millis())
    {
        eprintln!("tui-do: {err:#}");
    }
}

// ── Codex ──────────────────────────────────────────────────────

async fn codex_account() -> Usage {
    let Some(file) = crate::hooks::codex_home().map(|home| home.join("auth.json")) else {
        return Usage::unavailable("not signed in to codex");
    };
    let Ok(auth) = tokio::fs::read_to_string(file).await else {
        return Usage::unavailable("not signed in to codex");
    };
    let auth: Value = serde_json::from_str(&auth).unwrap_or_default();
    let tokens = &auth["tokens"];
    let (Some(token), Some(account)) = (
        tokens["access_token"].as_str(),
        tokens["account_id"].as_str(),
    ) else {
        return Usage::unavailable(if auth["OPENAI_API_KEY"].is_string() {
            "codex uses an api key, which has no plan limits"
        } else {
            "not signed in to codex"
        });
    };
    let config = [
        format!(
            "url = {}",
            quoted("https://chatgpt.com/backend-api/wham/usage")
        ),
        format!(
            "header = {}",
            quoted(&format!("authorization: Bearer {token}"))
        ),
        format!(
            "header = {}",
            quoted(&format!("chatgpt-account-id: {account}"))
        ),
        format!("header = {}", quoted("user-agent: tui-do")),
    ];
    match curl(&config, REQUEST_TIMEOUT).await {
        Ok((200, body)) => codex_usage(&body, now_millis())
            .unwrap_or_else(|| Usage::unavailable("chatgpt's answer wasn't understood")),
        Ok((401 | 403, body)) => {
            tracing::info!("codex usage: login refused: {}", reason(&body));
            Usage::unavailable("codex's login needs refreshing")
        }
        Ok((status, body)) => {
            tracing::warn!("codex usage: {status}: {}", reason(&body));
            Usage::unavailable(format!("chatgpt answered {status}"))
        }
        Err(err) => {
            tracing::warn!("codex usage: {err:#}");
            Usage::unavailable("couldn't reach chatgpt")
        }
    }
}

/// ChatGPT's answer: `plan_type`, and `rate_limit`'s `primary_window` and
/// `secondary_window`, each with a `used_percent`, its length in
/// `limit_window_seconds` and `reset_at` in seconds.
fn codex_usage(body: &str, now: i64) -> Option<Usage> {
    let json: Value = serde_json::from_str(body).ok()?;
    let limits = &json["rate_limit"];
    let windows: Vec<Window> = ["primary_window", "secondary_window"]
        .into_iter()
        .filter_map(|key| {
            let window = &limits[key];
            Some(Window {
                label: window_label(window["limit_window_seconds"].as_i64()?),
                used_percent: window["used_percent"].as_f64()?,
                resets_at: window["reset_at"].as_i64().map(|secs| secs * 1000),
            })
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    Some(Usage::Ok {
        plan: json["plan_type"].as_str().map(str::to_string),
        as_of: now,
        source: Source::Account,
        machine: None,
        windows,
    })
}

/// "5-hour", "weekly", "2-day", …
fn window_label(seconds: i64) -> String {
    let hours = (seconds + 1800) / 3600;
    match hours {
        168 => "weekly".to_string(),
        h if h > 0 && h % 24 == 0 => format!("{}-day", h / 24),
        h => format!("{}-hour", h.max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(as_of: i64) -> Usage {
        Usage::Ok {
            plan: None,
            as_of,
            source: Source::Account,
            machine: None,
            windows: vec![],
        }
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tui-do-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn anthropic_usage_should_give_its_windows() {
        let body = r#"{"five_hour":{"utilization":23.0,"resets_at":"2026-11-12T15:00:00+00:00"},
            "seven_day":{"utilization":41.5,"resets_at":null},"seven_day_opus":null}"#;
        let windows = claude_windows(body).expect("windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5-hour");
        assert_eq!(windows[0].used_percent, 23.0);
        assert_eq!(windows[0].resets_at, Some(1_794_495_600_000));
        assert_eq!(windows[1].label, "weekly");
        assert_eq!(windows[1].resets_at, None);
    }

    #[test]
    fn a_status_line_with_limits_should_give_its_windows() {
        let input = json!({ "rate_limits": {
            "five_hour": { "used_percentage": 12.5, "resets_at": 1_794_495_600 },
            "seven_day": { "used_percentage": 40, "resets_at": 1_794_851_715 },
        }});
        let windows = status_line_windows(&input).expect("windows");
        assert_eq!(windows[0].used_percent, 12.5);
        assert_eq!(windows[0].resets_at, Some(1_794_495_600_000));
        assert_eq!(windows[1].label, "weekly");
        assert_eq!(windows[1].used_percent, 40.0);
    }

    #[test]
    fn a_status_line_without_limits_should_give_nothing() {
        assert!(status_line_windows(&json!({ "model": { "id": "claude-opus-5-5" } })).is_none());
    }

    #[test]
    fn chatgpt_usage_should_give_its_plan_and_windows() {
        let body = r#"{"plan_type":"plus","rate_limit":{
            "primary_window":{"used_percent":7,"limit_window_seconds":18000,"reset_at":1794507251},
            "secondary_window":{"used_percent":2,"limit_window_seconds":604800,"reset_at":1794851715}}}"#;
        let Some(Usage::Ok { plan, windows, .. }) = codex_usage(body, 1) else {
            panic!("no usage");
        };
        assert_eq!(plan.as_deref(), Some("plus"));
        assert_eq!(windows[0].label, "5-hour");
        assert_eq!(windows[0].used_percent, 7.0);
        assert_eq!(windows[0].resets_at, Some(1_794_507_251_000));
        assert_eq!(windows[1].label, "weekly");
    }

    #[test]
    fn chatgpt_usage_without_limits_should_give_nothing() {
        assert!(codex_usage(r#"{"plan_type":"free","rate_limit":null}"#, 1).is_none());
    }

    #[test]
    fn windows_should_be_named_by_their_length() {
        assert_eq!(window_label(18_000), "5-hour");
        assert_eq!(window_label(604_800), "weekly");
        assert_eq!(window_label(172_800), "2-day");
    }

    #[test]
    fn the_newer_answer_should_win() {
        assert_eq!(freshest(ok(1), ok(2)), ok(2));
        assert_eq!(freshest(ok(2), ok(1)), ok(2));
        assert_eq!(freshest(Usage::unavailable("no"), ok(1)), ok(1));
        assert_eq!(freshest(ok(1), Usage::unavailable("no")), ok(1));
        assert_eq!(
            freshest(Usage::unavailable("a"), Usage::unavailable("b")),
            Usage::unavailable("a")
        );
    }

    #[test]
    fn a_window_that_started_over_should_read_as_unused() {
        let usage = Usage::Ok {
            plan: None,
            as_of: 1,
            source: Source::StatusLine,
            machine: None,
            windows: vec![
                Window {
                    label: "5-hour".into(),
                    used_percent: 80.0,
                    resets_at: Some(100),
                },
                Window {
                    label: "weekly".into(),
                    used_percent: 30.0,
                    resets_at: Some(900),
                },
            ],
        };
        let Usage::Ok { windows, .. } = usage.aged(500) else {
            panic!("unavailable");
        };
        assert_eq!((windows[0].used_percent, windows[0].resets_at), (0.0, None));
        assert_eq!(
            (windows[1].used_percent, windows[1].resets_at),
            (30.0, Some(900))
        );
    }

    #[test]
    fn recorded_limits_should_read_back_from_the_status_line() {
        let dir = temp();
        let windows = vec![Window {
            label: "5-hour".into(),
            used_percent: 5.0,
            resets_at: None,
        }];
        record(&dir, windows.clone(), 1_000).unwrap();
        let Some(Usage::Ok {
            as_of,
            source,
            windows: read,
            ..
        }) = status_line_record(&dir)
        else {
            panic!("nothing recorded");
        };
        assert_eq!(
            (as_of, source, read),
            (1_000, Source::StatusLine, windows.clone())
        );
        // The same limits soon after aren't saved again; a minute on, they are
        record(&dir, windows.clone(), 2_000).unwrap();
        assert_eq!(
            status_line_record(&dir).and_then(|u| u.as_of()),
            Some(1_000)
        );
        record(&dir, windows, 1_000 + RESAVE_AFTER_MS).unwrap();
        assert_eq!(
            status_line_record(&dir).and_then(|u| u.as_of()),
            Some(1_000 + RESAVE_AFTER_MS)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_projects_own_status_line_should_come_first() {
        let project = temp();
        let claude = project.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"statusLine":{"type":"command","command":"shared"}}"#,
        )
        .unwrap();
        let input = json!({ "workspace": { "project_dir": project } });
        assert_eq!(own_status_line(&input).as_deref(), Some("shared"));
        std::fs::write(
            claude.join("settings.local.json"),
            r#"{"statusLine":{"type":"command","command":"local"}}"#,
        )
        .unwrap();
        assert_eq!(own_status_line(&input).as_deref(), Some("local"));
        std::fs::remove_dir_all(project).ok();
    }
}
