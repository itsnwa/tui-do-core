//! The TUI-DO MCP tools: the same names, descriptions and parameters as the
//! original Node server, so agent instructions written for it keep working,
//! backed by the workspace module.

use rmcp::model::{JsonObject, Tool};
use serde_json::{Value as Json, json};

use crate::links::{SessionLink, TaskSessions};
use crate::workspace::plans::SavePlan;
use crate::workspace::projects::{CreateProject, UpdateProject};
use crate::workspace::schedule::Autostart;
use crate::workspace::tasks::{CreateTask, STATES, TaskFilter, UpdateTask};
use crate::workspace::{Error, Workspace};

const TASK_ID: &str = "Task ID — accepts any format: \"58\", \"058\", or \"26-058\". Plain numbers are auto-prefixed with the current year.";

const START_TASK: &str = "Start working on a task: marks it in-progress and returns its description, attachments (file paths), and plan. Call this first when the user asks you to work on a task (e.g. \"work on task 029\"), before exploring code or planning.

- Finds the task by ID (\"029\" or \"26-029\"; bare numbers resolve to the current year)
- If expected_project is given and doesn't match, returns a PROJECT_MISMATCH warning and starts nothing
- Reserves a plan for the task if it has none (write it with update_plan)

If it returns an error or a warning, or the task isn't the one the user meant, stop and ask the user instead of guessing another task.";

const COMPLETE_TASK: &str = "Mark a task as completed (a recurring task instead moves to its next due date and back to not-started). Only call this after the user has confirmed the work looks good.

A summary is required. Describe:
- What was done (key changes, files modified)
- How it works (brief technical approach)
- Any important decisions or trade-offs made
It's stored in the task's plan as \"## Completion Summary\" (replacing an earlier one), and get_task returns it to future agents working on related tasks.";

const UPDATE_PLAN: &str = "Write the plan document linked to a task: your implementation plan, progress notes, or revisions. This is TUI-DO's plan, not a built-in plan or todo tool.

It replaces the whole document, so include any existing content that still applies (start_task returns the current plan). Save your plan here before implementing, and again when it changes, so it's visible in the TUI-DO app. Only write to the task you're working on; in plan mode, save it right after the plan is approved.";

const LIST_TASKS: &str = "List tasks, optionally filtered by project, state, or due date. Completed tasks are left out unless you filter by state or due date, or set include_completed. Returns task IDs in YY-NNN format (e.g. 26-058). When the user refers to 'task 58', pass '58' to other tools — it auto-resolves to the current year ('26-058').";

/// One parameter: name, JSON type, description, required. A type of
/// `"state"` is a string enum, `"recurrence"` a string pattern and
/// `"autostart"` an object; a `?` after one also allows null.
type Param = (&'static str, &'static str, &'static str, bool);

/// What a task's `recurrence` can be.
const RECURRENCE_PATTERN: &str = "^(daily|weekly|monthly|yearly|weekdays|days:(mon|tue|wed|thu|fri|sat|sun)(,(mon|tue|wed|thu|fri|sat|sun))*|hours:([1-9]|1[0-9]|2[0-4]))$";

const RECURRENCE: &str = "Recurrence pattern (requires due date): daily, weekly, monthly, yearly, weekdays (Monday to Friday), days:mon,wed,fri (those days of the week) or hours:N (every N hours from `at`, N from 1 to 24)";

const AT: &str = "Time of day it's due, HH:MM (24-hour), in `timezone`. Without one, a task with autostart starts at midnight.";

const TIMEZONE: &str = "IANA time zone `at` is in, e.g. Europe/Oslo (default: the home machine's)";

const AUTOSTART: &str = "Start a coding agent on the task when it comes due (at `due` and `at`, and again at each recurrence). Occurrences that have already passed when this is set don't start one. A run doesn't start while the previous one is still open; it's counted as skipped instead.";

/// The JSON schema of `autostart`, with snake_case keys like the other parameters.
fn autostart_schema(description: &str) -> Json {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "agent": { "type": "string", "enum": ["claude", "codex"], "description": "The agent (default: claude)" },
            "model": { "type": "string", "description": "Claude's --model value, or a Codex model slug (default: the agent's)" },
            "effort": { "type": "string", "description": "Effort level, e.g. high" },
            "permission_mode": { "type": "string", "description": "Claude's --permission-mode, e.g. auto or acceptEdits. A run that hits a permission prompt waits for the user." },
            "yolo": { "type": "boolean", "description": "Skip permission prompts (and Codex's sandbox)" },
            "device": { "type": "string", "description": "Fleet machine id to run on (default: the machine with the projects' folders)" },
            "complete_on_its_own": { "type": "boolean", "description": "The agent completes the task when it's done instead of asking the user first. For jobs whose result is a summary or new tasks rather than code." },
        },
    })
}

/// `autostart` as the agent passed it.
fn autostart_of(value: &Json) -> Autostart {
    let text = |key: &str| {
        value[key]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let flag = |key: &str| value[key].as_bool().unwrap_or(false);
    Autostart {
        agent: text("agent"),
        model: text("model"),
        effort: text("effort"),
        permission_mode: text("permission_mode"),
        yolo: flag("yolo"),
        device: text("device"),
        complete_on_its_own: flag("complete_on_its_own"),
    }
}

fn schema(params: &[Param]) -> JsonObject {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for &(name, kind, description, is_required) in params {
        let prop = match kind {
            "state" => json!({ "type": "string", "enum": STATES, "description": description }),
            "recurrence" => {
                json!({ "type": "string", "pattern": RECURRENCE_PATTERN, "description": description })
            }
            "recurrence?" => json!({
                "anyOf": [{ "type": "string", "pattern": RECURRENCE_PATTERN }, { "type": "null" }],
                "description": description,
            }),
            "string?" => json!({
                "anyOf": [{ "type": "string" }, { "type": "null" }],
                "description": description,
            }),
            "autostart" => autostart_schema(description),
            "autostart?" => json!({
                "anyOf": [autostart_schema(description), { "type": "null" }],
                "description": description,
            }),
            other => json!({ "type": other, "description": description }),
        };
        properties.insert(name.into(), prop);
        if is_required {
            required.push(name);
        }
    }
    let mut object = serde_json::Map::new();
    object.insert("type".into(), "object".into());
    object.insert("properties".into(), properties.into());
    if !required.is_empty() {
        object.insert("required".into(), json!(required));
    }
    object
}

pub fn definitions() -> Vec<Tool> {
    let tool = |name: &'static str, description: &'static str, params: &[Param]| {
        Tool::new(name, description, schema(params))
    };
    vec![
        tool(
            "start_task",
            START_TASK,
            &[
                ("task_id", "string", TASK_ID, true),
                (
                    "expected_project",
                    "string",
                    "Project slug the AI expects this task to belong to (for validation)",
                    false,
                ),
            ],
        ),
        tool(
            "complete_task",
            COMPLETE_TASK,
            &[
                ("task_id", "string", TASK_ID, true),
                (
                    "summary",
                    "string",
                    "Completion summary to append to the plan (REQUIRED — task will not be completed without it)",
                    false,
                ),
            ],
        ),
        tool(
            "update_plan",
            UPDATE_PLAN,
            &[
                ("task_id", "string", TASK_ID, true),
                (
                    "content",
                    "string",
                    "Full plan content (markdown). Replaces existing content.",
                    true,
                ),
            ],
        ),
        tool("list_projects", "List all projects in the workspace", &[]),
        tool(
            "create_project",
            "Create a new project",
            &[
                ("title", "string", "Project title", true),
                (
                    "directory",
                    "string",
                    "Absolute path to the project directory",
                    false,
                ),
                (
                    "remote",
                    "string",
                    "Git remote URL the code can be fetched from (e.g. https://github.com/owner/repo)",
                    false,
                ),
            ],
        ),
        tool(
            "update_project",
            "Update a project's properties",
            &[
                ("slug", "string", "Project slug", true),
                ("title", "string", "New title", false),
                (
                    "muted",
                    "boolean",
                    "Whether to mute the project from weekly updates",
                    false,
                ),
                (
                    "archived",
                    "boolean",
                    "Whether the project is archived (hidden from the normal views, restorable)",
                    false,
                ),
                (
                    "directory",
                    "string",
                    "Absolute path to the project directory",
                    false,
                ),
                (
                    "remote",
                    "string",
                    "Git remote URL the code can be fetched from (e.g. https://github.com/owner/repo)",
                    false,
                ),
            ],
        ),
        tool(
            "delete_project",
            "Delete a project along with its tasks, and their plans and attachments. This can't be undone.",
            &[("slug", "string", "Project slug", true)],
        ),
        tool(
            "list_tasks",
            LIST_TASKS,
            &[
                ("project", "string", "Filter by project slug", false),
                ("state", "state", "Filter by state", false),
                ("due", "string", "Filter by due date (YYYY-MM-DD)", false),
                (
                    "include_completed",
                    "boolean",
                    "Also return completed tasks when not filtering by state or due date",
                    false,
                ),
            ],
        ),
        tool(
            "get_task",
            "Get a specific task by slug or short ID. For completed tasks, also returns the plan content including completion summary for context.",
            &[(
                "identifier",
                "string",
                "Task slug or ID — accepts any format: '58', '058', '26-058', or 'my-task-slug'. Plain numbers are auto-prefixed with the current year.",
                true,
            )],
        ),
        tool(
            "create_task",
            "Create a new task with an auto-generated sequential ID (YY-NNN format)",
            &[
                ("title", "string", "Task title", true),
                ("project", "string", "Project slug to associate with", false),
                ("due", "string", "Due date (YYYY-MM-DD)", false),
                ("recurrence", "recurrence", RECURRENCE, false),
                ("at", "string", AT, false),
                ("timezone", "string", TIMEZONE, false),
                ("autostart", "autostart", AUTOSTART, false),
                (
                    "description",
                    "string",
                    "Task description (markdown)",
                    false,
                ),
            ],
        ),
        tool(
            "update_task",
            "Update a task's properties or state",
            &[
                ("slug", "string", "Task slug", true),
                ("title", "string", "New title", false),
                ("state", "state", "New state", false),
                ("project", "string", "New project slug", false),
                ("due", "string", "New due date (YYYY-MM-DD)", false),
                (
                    "recurrence",
                    "recurrence?",
                    "Recurrence pattern (null to remove): daily, weekly, monthly, yearly, weekdays, days:mon,wed,fri or hours:N (1 to 24)",
                    false,
                ),
                (
                    "at",
                    "string?",
                    "Time of day it's due, HH:MM (null to remove)",
                    false,
                ),
                (
                    "timezone",
                    "string?",
                    "IANA time zone `at` is in (null: the home machine's)",
                    false,
                ),
                (
                    "autostart",
                    "autostart?",
                    "Start a coding agent on the task when it comes due (null to stop). See create_task.",
                    false,
                ),
                ("plan", "string", "Associated plan slug", false),
                ("description", "string", "New description (markdown)", false),
            ],
        ),
        tool(
            "delete_task",
            "Delete a task",
            &[("slug", "string", "Task slug", true)],
        ),
        tool("list_plans", "List all plans", &[]),
        tool(
            "get_plan",
            "Get a plan's content",
            &[("slug", "string", "Plan slug", true)],
        ),
        tool(
            "save_plan",
            "Create or update a plan",
            &[
                ("slug", "string", "Plan slug (filename without .md)", true),
                ("title", "string", "Plan title", false),
                ("task", "string", "Associated task slug", false),
                ("content", "string", "Plan content (markdown)", true),
            ],
        ),
        tool(
            "delete_plan",
            "Delete a plan",
            &[("slug", "string", "Plan slug", true)],
        ),
        tool(
            "get_daily_note",
            "Get or create a daily note for a specific date. Also returns tasks due on that date.",
            &[("date", "string", "Date in YYYY-MM-DD format", true)],
        ),
        tool(
            "save_daily_note",
            "Save content to a daily note",
            &[
                ("date", "string", "Date in YYYY-MM-DD format", true),
                ("content", "string", "Note content (markdown)", true),
            ],
        ),
        tool("list_daily_notes", "List all daily notes", &[]),
        tool("list_weekly_updates", "List all weekly updates", &[]),
        tool(
            "get_weekly_update",
            "Get a weekly update by week ID",
            &[(
                "weekId",
                "string",
                "Week ID in YYYY-WXX format (e.g., 2026-W08)",
                true,
            )],
        ),
        tool(
            "save_weekly_update",
            "Save or update a weekly summary",
            &[
                ("weekId", "string", "Week ID in YYYY-WXX format", true),
                (
                    "content",
                    "string",
                    "Weekly update content (markdown)",
                    true,
                ),
            ],
        ),
    ]
}

struct Args(JsonObject);

impl Args {
    fn opt(&self, key: &str) -> Option<String> {
        self.0.get(key).and_then(Json::as_str).map(str::to_string)
    }

    fn req(&self, key: &str) -> Result<String, String> {
        self.opt(key)
            .ok_or_else(|| format!("Invalid arguments: \"{key}\" is required and must be a string"))
    }

    /// Absent → `None`, `null` → `Some(None)`.
    fn nullable(&self, key: &str) -> Option<Option<String>> {
        self.0.get(key).map(|v| v.as_str().map(str::to_string))
    }

    fn state(&self, key: &str) -> Result<Option<String>, String> {
        match self.opt(key) {
            Some(s) if !STATES.contains(&s.as_str()) => Err(format!(
                "Invalid arguments: \"{key}\" must be one of {}",
                STATES.join(", ")
            )),
            other => Ok(other),
        }
    }
}

fn to_json(value: impl serde::Serialize) -> Result<Json, Error> {
    serde_json::to_value(value).map_err(|e| Error::Io(e.into()))
}

/// Run a tool. `Ok` is the tool's JSON result (business errors such as an
/// unknown task are results with an `error` field, as before); `Err` is a
/// failed call, reported to the agent as an error result.
pub fn call(
    ws: &Workspace,
    sessions: &TaskSessions,
    name: &str,
    arguments: JsonObject,
    caller: Option<&SessionLink>,
) -> Result<Json, String> {
    let args = Args(arguments);
    let result = match name {
        "start_task" => {
            let reference = args.req("task_id")?;
            let result = ws.start_task(&reference, args.opt("expected_project").as_deref());
            if let (Ok(started), Some(link)) = (&result, caller)
                && started["status"] == "started"
                && let Some(slug) = started["task"]["slug"].as_str()
                && let Err(err) = sessions.set(slug, link)
            {
                tracing::warn!(%err, slug, "couldn't link the Claude session");
            }
            result
        }
        "complete_task" => ws.complete_task(&args.req("task_id")?, args.opt("summary").as_deref()),
        "update_plan" => ws.write_task_plan(&args.req("task_id")?, &args.req("content")?),
        "list_projects" => ws.list_projects().and_then(to_json),
        "create_project" => {
            let req = CreateProject {
                title: args.req("title")?,
                directory: args.opt("directory"),
                remote: args.opt("remote"),
            };
            ws.create_project(req).and_then(to_json)
        }
        "update_project" => {
            let req = UpdateProject {
                title: args.opt("title"),
                muted: args.0.get("muted").and_then(Json::as_bool),
                archived: args.0.get("archived").and_then(Json::as_bool),
                directory: args.opt("directory"),
                remote: args.opt("remote"),
            };
            ws.update_project(&args.req("slug")?, req).and_then(to_json)
        }
        "delete_project" => {
            let slug = args.req("slug")?;
            ws.delete_project(&slug)
                .map(|()| json!({ "deleted": slug }))
        }
        "list_tasks" => {
            let state = args.state("state")?;
            let due = args.opt("due");
            let include_completed = args.0.get("include_completed").and_then(Json::as_bool);
            // Completed tasks are most of the list, so an agent has to ask for them
            let hide_completed =
                state.is_none() && due.is_none() && include_completed != Some(true);
            let filter = TaskFilter {
                project: args.opt("project"),
                state,
                due,
                completed_within: hide_completed.then_some(0),
            };
            ws.list_tasks(&filter).and_then(to_json)
        }
        "get_task" => {
            let identifier = args.req("identifier")?;
            match ws.find_task(&identifier) {
                Ok(Some(slug)) => ws.get_task(&slug).and_then(|mut task| {
                    task.content = None;
                    let mut value = to_json(&task)?;
                    if task.task.state == "completed"
                        && let Some(plan) = task.task.plan.as_deref().filter(|p| !p.is_empty())
                        && let Ok(plan) = ws.get_plan(plan)
                        && !plan.created.is_empty()
                    {
                        value["planContent"] = plan.content.unwrap_or_default().into();
                    }
                    Ok(value)
                }),
                Ok(None) => Ok(json!({ "error": format!("Task not found: \"{identifier}\"") })),
                Err(err) => Err(err),
            }
        }
        "create_task" => {
            let req = CreateTask {
                title: args.req("title")?,
                project: args.opt("project"),
                due: args.opt("due"),
                recurrence: args.opt("recurrence"),
                description: args.opt("description"),
                at: args.opt("at"),
                tz: args.opt("timezone"),
                autostart: args
                    .0
                    .get("autostart")
                    .filter(|a| a.is_object())
                    .map(autostart_of),
            };
            ws.create_task(req).and_then(to_json)
        }
        "update_task" => {
            let reference = args.req("slug")?;
            let slug = ws
                .find_task(&reference)
                .map_err(|e| e.to_string())?
                .unwrap_or(reference);
            let req = UpdateTask {
                title: args.opt("title"),
                state: args.state("state")?,
                project: args.opt("project"),
                due: args.opt("due").map(|d| Some(d).filter(|d| !d.is_empty())),
                recurrence: args.nullable("recurrence"),
                plan: args.opt("plan").map(Some),
                description: args.opt("description"),
                device: None,
                pr: None,
                at: args.nullable("at"),
                tz: args.nullable("timezone"),
                autostart: args
                    .0
                    .get("autostart")
                    .map(|a| a.is_object().then(|| autostart_of(a))),
            };
            ws.update_task(&slug, req).and_then(to_json)
        }
        "delete_task" => {
            let slug = args.req("slug")?;
            ws.delete_task(&slug).map(|_| json!({ "deleted": slug }))
        }
        "list_plans" => ws.list_plans().and_then(to_json),
        "get_plan" => ws.get_plan(&args.req("slug")?).and_then(to_json),
        "save_plan" => {
            let req = SavePlan {
                title: args.opt("title"),
                task: args.opt("task").map(Some),
                content: args.req("content")?,
            };
            ws.save_plan(&args.req("slug")?, req).and_then(to_json)
        }
        "delete_plan" => {
            let slug = args.req("slug")?;
            ws.delete_plan(&slug).map(|()| json!({ "deleted": slug }))
        }
        "get_daily_note" => ws.get_daily_note(&args.req("date")?).and_then(to_json),
        "save_daily_note" => {
            let date = args.req("date")?;
            ws.save_daily_note(&date, &args.req("content")?)
                .map(|()| json!({ "date": date, "saved": true }))
        }
        "list_daily_notes" => ws.list_daily_notes().and_then(to_json),
        "list_weekly_updates" => ws.list_weekly_updates().and_then(to_json),
        "get_weekly_update" => ws.get_weekly_update(&args.req("weekId")?).and_then(to_json),
        "save_weekly_update" => {
            let week = args.req("weekId")?;
            ws.save_weekly_update(&week, &args.req("content")?, true)
                .map(|generated| json!({ "weekId": week, "generated": generated }))
        }
        other => return Err(format!("Unknown tool: {other}")),
    };
    result.map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_should_be_the_original_22_tools() {
        assert_eq!(definitions().len(), 22);
    }

    #[test]
    fn an_agent_should_be_able_to_schedule_a_task() {
        let root = std::env::temp_dir().join(format!("tui-do-mcp-{}", uuid::Uuid::new_v4()));
        let ws = Workspace::open(root.clone()).expect("open");
        let sessions = TaskSessions::new(root.join("sessions"));
        let call = |name: &str, args: Json| {
            let Json::Object(args) = args else {
                panic!("arguments must be an object")
            };
            call(&ws, &sessions, name, args, None)
        };
        let task = call(
            "create_task",
            json!({
                "title": "Check the community reports",
                "due": "2099-01-05",
                "recurrence": "weekdays",
                "at": "09:00",
                "timezone": "Europe/Oslo",
                "autostart": { "permission_mode": "auto", "complete_on_its_own": true },
            }),
        )
        .expect("created");
        assert_eq!(task["recurrence"], "weekdays");
        assert_eq!(
            task["autostart"],
            json!({ "permissionMode": "auto", "completeOnItsOwn": true })
        );
        let off = call(
            "update_task",
            json!({
                "slug": task["slug"], "autostart": null, "at": null,
            }),
        )
        .expect("updated");
        assert!(
            off.get("autostart").is_none() && off.get("at").is_none(),
            "{off}"
        );
        let odd = json!({ "title": "Odd", "due": "2099-01-05", "recurrence": "fortnightly" });
        assert!(call("create_task", odd).is_err());
        std::fs::remove_dir_all(root).expect("clean up");
    }

    #[test]
    fn required_parameters_should_be_listed() {
        let tools = definitions();
        let start = tools
            .iter()
            .find(|t| t.name == "start_task")
            .expect("start_task");
        assert_eq!(start.input_schema["required"], json!(["task_id"]));
    }

    #[test]
    fn list_tasks_should_leave_out_completed_tasks_unless_asked() {
        let root = std::env::temp_dir().join(format!("tui-do-mcp-{}", uuid::Uuid::new_v4()));
        let ws = Workspace::open(root.clone()).expect("open");
        let sessions = TaskSessions::new(root.join("sessions"));
        let add = |title: &str| {
            ws.create_task(CreateTask {
                title: title.into(),
                due: Some("2026-01-01".into()),
                ..CreateTask::default()
            })
            .expect("create")
            .task
            .slug
        };
        add("Open");
        let done = add("Done");
        ws.update_task(
            &done,
            UpdateTask {
                state: Some("completed".into()),
                ..UpdateTask::default()
            },
        )
        .expect("complete");
        let count = |args: Json| {
            let Json::Object(args) = args else {
                panic!("arguments must be an object")
            };
            let tasks = call(&ws, &sessions, "list_tasks", args, None).expect("list");
            tasks.as_array().expect("array").len()
        };

        assert_eq!(count(json!({})), 1);
        assert_eq!(count(json!({ "include_completed": true })), 2);
        assert_eq!(count(json!({ "state": "completed" })), 1);
        assert_eq!(count(json!({ "due": "2026-01-01" })), 2);
        std::fs::remove_dir_all(root).expect("clean up");
    }
}
