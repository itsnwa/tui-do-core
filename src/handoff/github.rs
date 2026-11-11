//! Pull requests, through the `gh` CLI and its login on this machine: open
//! one for a task's pushed branch, look at it, and merge it.

use std::process::Command;

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::Value;

/// Where `gh` lives on macOS and Linux, for a core started without the
/// user's PATH.
const GH_CLIS: [&str; 4] = [
    "gh",
    "/opt/homebrew/bin/gh",
    "/usr/local/bin/gh",
    "/usr/bin/gh",
];

fn gh(args: &[&str]) -> anyhow::Result<String> {
    for cli in GH_CLIS {
        // Not in a repo: every call names the repo or the pull request
        let Ok(out) = Command::new(cli)
            .args(args)
            .current_dir(std::env::temp_dir())
            .env("GH_PROMPT_DISABLED", "1")
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("gh {} failed: {}", args.join(" "), stderr.trim());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    bail!("the GitHub CLI (gh) isn't installed on this machine")
}

/// Open a pull request for `head` into `base` on `repo` (`owner/repo`), and
/// return its URL.
pub fn create_pr(
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> anyhow::Result<String> {
    let out = gh(&[
        "pr", "create", "--repo", repo, "--head", head, "--base", base, "--title", title, "--body",
        body,
    ])?;
    out.lines()
        .rev()
        .find(|line| line.starts_with("https://"))
        .map(str::to_string)
        .context("gh didn't say where the pull request is")
}

/// A pull request as the apps show it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub url: String,
    pub number: u64,
    pub title: String,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    pub draft: bool,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    /// `passing`, `failing`, `pending`, or `none` when there are no checks.
    pub checks: &'static str,
}

pub fn view_pr(url: &str) -> anyhow::Result<PullRequest> {
    valid_url(url)?;
    let out = gh(&[
        "pr",
        "view",
        url,
        "--json",
        "url,number,title,state,isDraft,mergeable,statusCheckRollup",
    ])?;
    let pr: Value = serde_json::from_str(&out).context("unexpected answer from gh")?;
    let text = |key: &str| pr[key].as_str().unwrap_or_default().to_string();
    Ok(PullRequest {
        url: text("url"),
        number: pr["number"].as_u64().unwrap_or_default(),
        title: text("title"),
        state: text("state"),
        draft: pr["isDraft"].as_bool().unwrap_or_default(),
        mergeable: text("mergeable"),
        checks: checks(&pr["statusCheckRollup"]),
    })
}

/// Squash-merge a pull request and delete its branch.
pub fn merge_pr(url: &str) -> anyhow::Result<()> {
    valid_url(url)?;
    gh(&["pr", "merge", url, "--squash", "--delete-branch"]).map(|_| ())
}

fn valid_url(url: &str) -> anyhow::Result<()> {
    if url.starts_with("https://") && !url.contains(char::is_whitespace) {
        Ok(())
    } else {
        bail!("not a pull request URL: {url:?}")
    }
}

/// The checks' overall state: check runs have a status and a conclusion,
/// commit statuses a state.
fn checks(rollup: &Value) -> &'static str {
    let Some(items) = rollup.as_array().filter(|items| !items.is_empty()) else {
        return "none";
    };
    let mut pending = false;
    for item in items {
        let outcome = item["conclusion"]
            .as_str()
            .filter(|c| !c.is_empty())
            .or(item["state"].as_str())
            .unwrap_or_default();
        let done = item["status"].as_str().is_none_or(|s| s == "COMPLETED");
        match outcome {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => {
                return "failing";
            }
            "SUCCESS" | "NEUTRAL" | "SKIPPED" if done => {}
            _ => pending = true,
        }
    }
    if pending { "pending" } else { "passing" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checks_should_summarize_runs_and_statuses() {
        assert_eq!(checks(&json!([])), "none");
        assert_eq!(checks(&Value::Null), "none");
        let ok = json!({ "status": "COMPLETED", "conclusion": "SUCCESS" });
        let running = json!({ "status": "IN_PROGRESS", "conclusion": "" });
        let failed = json!({ "status": "COMPLETED", "conclusion": "FAILURE" });
        let status_ok = json!({ "state": "SUCCESS" });
        let status_waiting = json!({ "state": "PENDING" });
        assert_eq!(checks(&json!([ok, status_ok])), "passing");
        assert_eq!(checks(&json!([ok, running])), "pending");
        assert_eq!(checks(&json!([ok, status_waiting])), "pending");
        assert_eq!(checks(&json!([running, failed])), "failing");
    }
}
