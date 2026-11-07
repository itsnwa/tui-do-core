//! The TUI-DO MCP tools: the same names, descriptions and parameters as the
//! original Node server, so agent instructions written for it keep working,
//! backed by the workspace module.

use rmcp::model::{JsonObject, Tool};
use serde_json::{Value as Json, json};

use crate::links::{SessionLink, TaskSessions};
use crate::workspace::plans::SavePlan;
use crate::workspace::projects::{CreateProject, UpdateProject};
use crate::workspace::tasks::{CreateTask, STATES, TaskFilter, UpdateTask};
use crate::workspace::{Error, Workspace, dates};

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

const LIST_TASKS: &str = "List tasks, optionally filtered by project, state, or due date. Returns task IDs in YY-NNN format (e.g. 26-058). When the user refers to 'task 58', pass '58' to other tools — it auto-resolves to the current year ('26-058').";

/// One parameter: name, JSON type, description, required. A type of
/// `"state"` / `"recurrence"` is a string enum; `"recurrence?"` also allows null.
type Param = (&'static str, &'static str, &'static str, bool);

fn schema(params: &[Param]) -> JsonObject {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for &(name, kind, description, is_required) in params {
        let prop = match kind {
            "state" => json!({ "type": "string", "enum": STATES, "description": description }),
            "recurrence" => {
                json!({ "type": "string", "enum": dates::RECURRENCES, "description": description })
            }
            "recurrence?" => json!({
                "anyOf": [{ "type": "string", "enum": dates::RECURRENCES }, { "type": "null" }],
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
                    "directory",
                    "string",
                    "Absolute path to the project directory",
                    false,
                ),
            ],
        ),
        tool(
            "delete_project",
            "Delete a project",
            &[("slug", "string", "Project slug", true)],
        ),
        tool(
            "list_tasks",
            LIST_TASKS,
            &[
                ("project", "string", "Filter by project slug", false),
                ("state", "state", "Filter by state", false),
                ("due", "string", "Filter by due date (YYYY-MM-DD)", false),
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
                (
                    "recurrence",
                    "recurrence",
                    "Recurrence pattern (requires due date)",
                    false,
                ),
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
                    "Recurrence pattern (null to remove)",
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
                ssh: None,
            };
            ws.create_project(req).and_then(to_json)
        }
        "update_project" => {
            let req = UpdateProject {
                title: args.opt("title"),
                muted: args.0.get("muted").and_then(Json::as_bool),
                directory: args.opt("directory"),
                ssh: None,
            };
            ws.update_project(&args.req("slug")?, req).and_then(to_json)
        }
        "delete_project" => {
            let slug = args.req("slug")?;
            ws.delete_project(&slug)
                .map(|()| json!({ "deleted": slug }))
        }
        "list_tasks" => {
            let filter = TaskFilter {
                project: args.opt("project"),
                state: args.state("state")?,
                due: args.opt("due"),
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
            let recurrence = args.opt("recurrence");
            if recurrence
                .as_deref()
                .is_some_and(|r| !dates::RECURRENCES.contains(&r))
            {
                return Err(format!(
                    "Invalid arguments: \"recurrence\" must be one of {}",
                    dates::RECURRENCES.join(", ")
                ));
            }
            let req = CreateTask {
                title: args.req("title")?,
                project: args.opt("project"),
                due: args.opt("due"),
                recurrence,
                description: args.opt("description"),
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
    fn required_parameters_should_be_listed() {
        let tools = definitions();
        let start = tools
            .iter()
            .find(|t| t.name == "start_task")
            .expect("start_task");
        assert_eq!(start.input_schema["required"], json!(["task_id"]));
    }
}
