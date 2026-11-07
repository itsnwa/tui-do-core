//! Sets up the coding agents installed on this machine (Claude Code, Codex)
//! to use the TUI-DO tools: registers `tui-do-core mcp` in their own config,
//! writes the workflow instructions into their global instructions file, and
//! allows the tools in Claude Code. Registering it with the agent (rather than
//! passing a config per session) keeps it working across `/login` and restarts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Serialize;
use serde_json::{Value, json};

/// The heading the instructions section starts with; it runs to the next `## `.
const MARKER: &str = "## TUI-DO Task Workflow (MCP Server)";
const TOOL_PERMISSION: &str = "mcp__tui-do__*";

#[derive(Debug, Default, Serialize)]
pub struct Setup {
    pub claude: bool,
    pub codex: bool,
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn installed(cli: &str) -> bool {
    Command::new(cli)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run(cli: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new(cli)
        .args(args)
        .output()
        .with_context(|| format!("running {cli}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{cli} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Put `section` (which starts with [`MARKER`]) into a markdown file,
/// replacing an earlier version of it.
pub fn write_section(path: &Path, section: &str) -> anyhow::Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let section = section.trim_end();
    let updated = match current.find(MARKER) {
        Some(start) => {
            let body = start + MARKER.len();
            let end = current[body..]
                .find("\n## ")
                .map_or(current.len(), |i| body + i);
            format!("{}{section}\n{}", &current[..start], &current[end..])
        }
        None if current.trim().is_empty() => format!("{section}\n"),
        None => format!("{}\n\n{section}\n", current.trim_end()),
    };
    if updated != current {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, updated)?;
    }
    Ok(())
}

/// Allow the TUI-DO tools in Claude Code's user settings.
fn allow_tools(settings: &Path) -> anyhow::Result<()> {
    let mut json: Value = match std::fs::read_to_string(settings) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} isn't valid JSON", settings.display()))?,
        Err(_) => json!({}),
    };
    let object = json
        .as_object_mut()
        .context("Claude's settings aren't a JSON object")?;
    let permissions = object.entry("permissions").or_insert_with(|| json!({}));
    let allow = permissions
        .as_object_mut()
        .context("permissions isn't an object")?
        .entry("allow")
        .or_insert_with(|| json!([]));
    let list = allow
        .as_array_mut()
        .context("permissions.allow isn't a list")?;
    if !list.iter().any(|v| v == TOOL_PERMISSION) {
        list.push(TOOL_PERMISSION.into());
        if let Some(dir) = settings.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(settings, serde_json::to_string_pretty(&json)? + "\n")?;
    }
    Ok(())
}

/// Set up every agent installed here. `instructions` is the workflow section.
pub fn setup(instructions: &str) -> anyhow::Result<Setup> {
    let exe = std::env::current_exe().context("finding tui-do-core's own path")?;
    let exe = exe.to_string_lossy().into_owned();
    let home = home()?;
    let mut done = Setup::default();
    if installed("claude") {
        // Re-adding replaces an older registration (e.g. a different path)
        let _ = run("claude", &["mcp", "remove", "--scope", "user", "tui-do"]);
        run(
            "claude",
            &["mcp", "add", "--scope", "user", "tui-do", "--", &exe, "mcp"],
        )?;
        write_section(&home.join(".claude/CLAUDE.md"), instructions)?;
        allow_tools(&home.join(".claude/settings.json"))?;
        done.claude = true;
    }
    if installed("codex") {
        let _ = run("codex", &["mcp", "remove", "tui-do"]);
        run("codex", &["mcp", "add", "tui-do", "--", &exe, "mcp"])?;
        write_section(&home.join(".codex/AGENTS.md"), instructions)?;
        done.codex = true;
    }
    tracing::info!(
        claude = done.claude,
        codex = done.codex,
        "agents set up for TUI-DO"
    );
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_section_should_replace_only_its_own_section() {
        let path = std::env::temp_dir().join(format!("tui-do-agents-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            format!("# Mine\n\nkeep\n\n{MARKER}\n\nold\n\n## After\n\nalso keep\n"),
        )
        .expect("write");
        write_section(&path, &format!("{MARKER}\n\nnew")).expect("section");
        let text = std::fs::read_to_string(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            text,
            format!("# Mine\n\nkeep\n\n{MARKER}\n\nnew\n\n## After\n\nalso keep\n")
        );
    }
}
