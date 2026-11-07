//! Checks the format layer against the original app on a real workspace.
//!
//! `expected/` holds, for every `<dir>/<file>.md`, what the JS app's
//! parse → stringify(content.trim(), data) produces and the parsed data as
//! JSON (see the parity script in the core's local notes).
//!
//!   cargo run --release --example workspace_parity -- <workspace> <expected>

use std::path::Path;

use serde_json::Value as Json;
use tui_do_core::format::{Value, parse_file, stringify};

fn decode(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(items) => Value::Seq(items.iter().map(decode).collect()),
        Json::Object(obj) => match obj.get("$date").and_then(Json::as_i64) {
            Some(ms) => Value::Date(ms),
            None => Value::Map(obj.iter().map(|(k, v)| (k.clone(), decode(v))).collect()),
        },
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [workspace, expected] = args.as_slice() else {
        anyhow::bail!("usage: workspace_parity <workspace> <expected>");
    };
    let (mut total, mut bytes_differ, mut data_differ, mut errors) = (0, 0, 0, 0);
    for dir in ["tasks", "plans", "projects", "daily", "weekly"] {
        for entry in std::fs::read_dir(Path::new(workspace).join(dir))? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".md") {
                continue;
            }
            total += 1;
            let exp_dir = Path::new(expected).join(dir);
            let raw = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
            let doc = match parse_file(&raw) {
                Ok(doc) => doc,
                Err(err) => {
                    errors += 1;
                    println!("PARSE ERROR {dir}/{name}: {err}");
                    continue;
                }
            };
            let exp_data = decode(&serde_json::from_str(&std::fs::read_to_string(
                exp_dir.join(format!("{name}.json")),
            )?)?);
            if Value::Map(doc.data.clone()) != exp_data {
                data_differ += 1;
                println!(
                    "DATA {dir}/{name}\n  js:   {exp_data:?}\n  rust: {:?}",
                    doc.data
                );
            }
            let exp_bytes = std::fs::read_to_string(exp_dir.join(name))?;
            let ours = stringify(&doc.content, &doc.data);
            if ours != exp_bytes {
                bytes_differ += 1;
                println!("BYTES {dir}/{name}\n  js:   {exp_bytes:?}\n  rust: {ours:?}");
            }
        }
    }
    println!(
        "{total} files: {data_differ} data differences, {bytes_differ} byte differences, {errors} parse errors"
    );
    Ok(())
}
