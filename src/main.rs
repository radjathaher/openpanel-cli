mod command_tree;

use anyhow::{Context, Result, anyhow};
use clap::{Arg, ArgAction, ArgMatches, Command};
use command_tree::{ArgDef, CommandTree, Operation, Resource};
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::blocking::Client;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;
use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::connect;
use tungstenite::http::{HeaderName, HeaderValue};

type RequestParts = (Url, Vec<(String, String)>, Option<Value>);

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let tree = command_tree::load_command_tree();
    let cli = build_cli(&tree);
    let matches = cli.get_matches();

    if let Some(matches) = matches.subcommand_matches("list") {
        return handle_list(&tree, matches);
    }
    if let Some(matches) = matches.subcommand_matches("describe") {
        return handle_describe(&tree, matches);
    }
    if let Some(matches) = matches.subcommand_matches("tree") {
        return handle_tree(&tree, matches);
    }

    let base_url = matches
        .get_one::<String>("api_url")
        .cloned()
        .or_else(|| env::var("OPENPANEL_API_URL").ok())
        .unwrap_or_else(|| tree.base_url.clone());

    let client_id = matches
        .get_one::<String>("client_id")
        .cloned()
        .or_else(|| env::var("OPENPANEL_CLIENT_ID").ok())
        .context("OPENPANEL_CLIENT_ID missing")?;

    let client_secret = matches
        .get_one::<String>("client_secret")
        .cloned()
        .or_else(|| env::var("OPENPANEL_CLIENT_SECRET").ok())
        .context("OPENPANEL_CLIENT_SECRET missing")?;

    let timeout = matches.get_one::<u64>("timeout").copied().unwrap_or(30);
    let pretty = matches.get_flag("pretty");
    let raw = matches.get_flag("raw");
    let dry_run = matches.get_flag("dry_run");
    let body_override = matches.get_one::<String>("body").map(String::as_str);

    let (resource_name, resource_matches) = matches
        .subcommand()
        .ok_or_else(|| anyhow!("resource required"))?;

    if resource_name == "auth" {
        let ("doctor", doctor_matches) = resource_matches
            .subcommand()
            .ok_or_else(|| anyhow!("operation required"))?
        else {
            return Err(anyhow!("unknown auth operation"));
        };
        return handle_auth_doctor(
            &base_url,
            &client_id,
            &client_secret,
            timeout,
            pretty,
            doctor_matches,
        );
    }

    if resource_name == "events" {
        let ("query", query_matches) = resource_matches
            .subcommand()
            .ok_or_else(|| anyhow!("operation required"))?
        else {
            return Err(anyhow!("unknown events operation"));
        };
        return handle_events_query(
            &base_url,
            &client_id,
            &client_secret,
            timeout,
            query_matches,
        );
    }

    if resource_name == "analytics" {
        let ("funnel", funnel_matches) = resource_matches
            .subcommand()
            .ok_or_else(|| anyhow!("operation required"))?
        else {
            return Err(anyhow!("unknown analytics operation"));
        };
        return handle_funnel(
            &base_url,
            &client_id,
            &client_secret,
            timeout,
            funnel_matches,
        );
    }

    if resource_name == "export"
        && let Some(("events-bulk", bulk_matches)) = resource_matches.subcommand()
    {
        return handle_events_query(&base_url, &client_id, &client_secret, timeout, bulk_matches);
    }

    let resource = tree
        .resources
        .iter()
        .find(|res| res.name == resource_name)
        .ok_or_else(|| anyhow!("unknown resource {resource_name}"))?;

    let (group_name, op_name, op_matches) = if resource.groups.is_empty() {
        let (op_name, op_matches) = resource_matches
            .subcommand()
            .ok_or_else(|| anyhow!("operation required"))?;
        (None, op_name, op_matches)
    } else {
        let (group_name, group_matches) = resource_matches
            .subcommand()
            .ok_or_else(|| anyhow!("group required"))?;
        let (op_name, op_matches) = group_matches
            .subcommand()
            .ok_or_else(|| anyhow!("operation required"))?;
        (Some(group_name), op_name, op_matches)
    };

    let group_label = group_name
        .map(|name| format!("{name} "))
        .unwrap_or_default();
    let op = find_op(resource, group_name, op_name)
        .ok_or_else(|| anyhow!("unknown command {resource_name} {group_label}{op_name}"))?;

    let (url, headers, body) = build_request(op, &base_url, op_matches, body_override)?;

    if dry_run {
        let output = json!({
            "method": op.method,
            "url": url.as_str(),
            "headers": headers,
            "body": body,
        });
        return write_output(&output, pretty);
    }

    if op.method == "WS" {
        execute_websocket(url, headers)?;
        return Ok(());
    }

    let client = Client::builder()
        .user_agent("openpanel-cli")
        .timeout(Duration::from_secs(timeout))
        .build()
        .context("build http client")?;

    let mut req = client
        .request(op.method.parse()?, url)
        .header("openpanel-client-id", client_id)
        .header("openpanel-client-secret", client_secret)
        .header("accept", "application/json");

    for (key, value) in headers {
        req = req.header(key, value);
    }

    if let Some(body) = body {
        req = req.header("content-type", "application/json").json(&body);
    }

    let resp = req.send().context("send request")?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let value: Value = resp.json().context("decode json")?;

    if raw {
        let mut header_map = Map::new();
        for (key, value) in headers.iter() {
            let entry = header_map
                .entry(key.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(values) = entry {
                values.push(Value::String(value.to_str().unwrap_or("").to_string()));
            }
        }
        let output = json!({
            "status": status.as_u16(),
            "headers": header_map,
            "body": value,
        });
        if !status.is_success() {
            write_output(&output, pretty)?;
            return Err(anyhow!("http {}", status));
        }
        return write_output(&output, pretty);
    }

    if !status.is_success() {
        write_output(&value, pretty)?;
        return Err(anyhow!(
            "http {}{}",
            status,
            auth_error_hint(resource_name, status)
        ));
    }

    if resource_name == "export" && op_name == "events" && op_matches.get_flag("debug_pagination") {
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let start = op_matches.get_one::<String>("start").map(String::as_str);
        let end = op_matches.get_one::<String>("end").map(String::as_str);
        let outside = rows
            .iter()
            .filter(|row| {
                let created = row
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                start.is_some_and(|s| created < s) || end.is_some_and(|e| created > e)
            })
            .count();
        eprintln!(
            "pagination: meta={} rows={} outside_requested_window={}",
            value.get("meta").unwrap_or(&Value::Null),
            rows.len(),
            outside
        );
    }

    write_output(&value, pretty)
}

fn build_cli(tree: &CommandTree) -> Command {
    let mut cmd = Command::new("openpanel")
        .about("OpenPanel CLI (auto-generated)")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("api_url")
                .long("api-url")
                .value_name("URL")
                .global(true)
                .help("Override API base URL (default: https://api.openpanel.dev)"),
        )
        .arg(
            Arg::new("client_id")
                .long("client-id")
                .value_name("ID")
                .global(true)
                .help("OpenPanel client ID (env: OPENPANEL_CLIENT_ID)"),
        )
        .arg(
            Arg::new("client_secret")
                .long("client-secret")
                .value_name("SECRET")
                .global(true)
                .help("OpenPanel client secret (env: OPENPANEL_CLIENT_SECRET)"),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .value_name("SECONDS")
                .global(true)
                .value_parser(clap::value_parser!(u64))
                .help("HTTP timeout in seconds (default: 30)"),
        )
        .arg(
            Arg::new("pretty")
                .long("pretty")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Pretty-print JSON output"),
        )
        .arg(
            Arg::new("raw")
                .long("raw")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Return full HTTP response (status, headers, body)"),
        )
        .arg(
            Arg::new("dry_run")
                .long("dry-run")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Print request without executing"),
        )
        .arg(
            Arg::new("body")
                .long("body")
                .global(true)
                .value_name("JSON")
                .help("Provide full JSON body for POST/PATCH/PUT"),
        )
        .arg(
            Arg::new("header")
                .long("header")
                .global(true)
                .action(ArgAction::Append)
                .value_name("KEY=VALUE")
                .help("Add custom header (repeatable, KEY=VALUE or KEY:VALUE)"),
        );

    cmd = cmd.subcommand(
        Command::new("list")
            .about("List resources and operations")
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .help("Emit machine-readable JSON"),
            ),
    );

    cmd = cmd.subcommand(
        Command::new("describe")
            .about("Describe a specific operation")
            .arg(Arg::new("resource").required(true))
            .arg(Arg::new("group").required(false))
            .arg(Arg::new("op").required(true))
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .help("Emit machine-readable JSON"),
            ),
    );

    cmd = cmd.subcommand(
        Command::new("tree").about("Show full command tree").arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("Emit machine-readable JSON"),
        ),
    );

    cmd = cmd.subcommand(
        Command::new("auth")
            .about("Authentication diagnostics")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                Command::new("doctor")
                    .about("Classify the configured OpenPanel API client as root, read, or write/invalid")
                    .arg(Arg::new("project_id").long("project-id").required(true))
                    .arg(
                        Arg::new("range")
                            .long("range")
                            .default_value("today")
                            .help("Insights range used for the read probe"),
                    ),
            ),
    );

    for resource in &tree.resources {
        let mut res_cmd = Command::new(resource.name.clone())
            .about(resource.name.clone())
            .subcommand_required(true)
            .arg_required_else_help(true);

        if resource.groups.is_empty() {
            for op in &resource.ops {
                let mut op_cmd = Command::new(op.name.clone()).about(op_hint(op));
                for arg in &op.args {
                    op_cmd = op_cmd.arg(build_arg(arg));
                }
                if resource.name == "export" && op.name == "events" {
                    op_cmd = op_cmd.arg(
                        Arg::new("debug_pagination")
                            .long("debug-pagination")
                            .action(ArgAction::SetTrue)
                            .help("Print pagination diagnostics for this page"),
                    );
                }
                res_cmd = res_cmd.subcommand(op_cmd);
            }
            if resource.name == "export" {
                res_cmd = res_cmd.subcommand(build_events_query_cmd("events-bulk"));
            }
        } else {
            for group in &resource.groups {
                let mut group_cmd = Command::new(group.name.clone())
                    .about(group.name.clone())
                    .subcommand_required(true)
                    .arg_required_else_help(true);
                for op in &group.ops {
                    let mut op_cmd = Command::new(op.name.clone()).about(op_hint(op));
                    for arg in &op.args {
                        op_cmd = op_cmd.arg(build_arg(arg));
                    }
                    group_cmd = group_cmd.subcommand(op_cmd);
                }
                res_cmd = res_cmd.subcommand(group_cmd);
            }
        }

        cmd = cmd.subcommand(res_cmd);
    }

    cmd = cmd.subcommand(
        Command::new("events")
            .about("Event analytics workflows")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(build_events_query_cmd("query")),
    );

    cmd = cmd.subcommand(
        Command::new("analytics")
            .about("Analytics summaries")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(build_funnel_cmd()),
    );

    cmd
}

fn build_events_query_cmd(name: &'static str) -> Command {
    Command::new(name)
        .about("Paginate, exact-filter, project, and stream OpenPanel events")
        .arg(Arg::new("project_id").long("project-id").required(true))
        .arg(Arg::new("events").long("events").required(true))
        .arg(Arg::new("start").long("start"))
        .arg(Arg::new("end").long("end"))
        .arg(Arg::new("day").long("day").help("Local day as YYYY-MM-DD"))
        .arg(
            Arg::new("tz_offset")
                .long("tz-offset")
                .default_value("+00:00")
                .help("Fixed offset like +07:00"),
        )
        .arg(Arg::new("where").long("where"))
        .arg(Arg::new("select").long("select"))
        .arg(
            Arg::new("format")
                .long("format")
                .default_value("jsonl")
                .value_parser(["jsonl", "json", "csv", "table"]),
        )
        .arg(Arg::new("output").long("output"))
        .arg(
            Arg::new("limit")
                .long("limit")
                .default_value("1000")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(
            Arg::new("max_pages")
                .long("max-pages")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(
            Arg::new("debug")
                .long("debug")
                .action(ArgAction::SetTrue)
                .help("Write fetch diagnostics to stderr"),
        )
}

fn build_funnel_cmd() -> Command {
    Command::new("funnel")
        .about("Build a funnel from raw events")
        .arg(Arg::new("project_id").long("project-id").required(true))
        .arg(Arg::new("steps").long("steps").required(true))
        .arg(
            Arg::new("unit")
                .long("unit")
                .default_value("sessionId")
                .value_parser(["sessionId", "deviceId", "profileId"]),
        )
        .arg(Arg::new("start").long("start"))
        .arg(Arg::new("end").long("end"))
        .arg(Arg::new("day").long("day"))
        .arg(
            Arg::new("tz_offset")
                .long("tz-offset")
                .default_value("+00:00"),
        )
        .arg(Arg::new("where").long("where"))
        .arg(
            Arg::new("format")
                .long("format")
                .default_value("table")
                .value_parser(["json", "table"]),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .default_value("1000")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(
            Arg::new("max_pages")
                .long("max-pages")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(Arg::new("debug").long("debug").action(ArgAction::SetTrue))
}

fn op_hint(op: &Operation) -> String {
    if let Some(desc) = &op.description {
        desc.clone()
    } else {
        format!("{} {}", op.method, op.path)
    }
}

fn handle_list(tree: &CommandTree, matches: &ArgMatches) -> Result<()> {
    if matches.get_flag("json") {
        let mut out = Vec::new();
        for res in &tree.resources {
            if res.groups.is_empty() {
                let ops: Vec<String> = res.ops.iter().map(|op| op.name.clone()).collect();
                out.push(json!({"resource": res.name, "ops": ops}));
            } else {
                let groups: Vec<Value> = res
                    .groups
                    .iter()
                    .map(|group| {
                        let ops: Vec<String> = group.ops.iter().map(|op| op.name.clone()).collect();
                        json!({"group": group.name, "ops": ops})
                    })
                    .collect();
                out.push(json!({"resource": res.name, "groups": groups}));
            }
        }
        return write_output(&Value::Array(out), true);
    }

    for res in &tree.resources {
        write_stdout_line(&res.name)?;
        if res.groups.is_empty() {
            for op in &res.ops {
                write_stdout_line(&format!("  {}", op.name))?;
            }
        } else {
            for group in &res.groups {
                write_stdout_line(&format!("  {}", group.name))?;
                for op in &group.ops {
                    write_stdout_line(&format!("    {}", op.name))?;
                }
            }
        }
    }
    Ok(())
}

fn handle_describe(tree: &CommandTree, matches: &ArgMatches) -> Result<()> {
    let resource = matches
        .get_one::<String>("resource")
        .ok_or_else(|| anyhow!("resource required"))?;

    let group = matches.get_one::<String>("group");

    let op_name = matches
        .get_one::<String>("op")
        .ok_or_else(|| anyhow!("operation required"))?;

    let res = tree
        .resources
        .iter()
        .find(|res| res.name == *resource)
        .ok_or_else(|| anyhow!("unknown resource {resource}"))?;

    let group_label = group.map(|name| format!("{name} ")).unwrap_or_default();
    let op = find_op(res, group.map(String::as_str), op_name)
        .ok_or_else(|| anyhow!("unknown command {resource} {group_label}{op_name}"))?;

    if matches.get_flag("json") {
        return write_output(&serde_json::to_value(op)?, true);
    }

    let mut label = resource.to_string();
    if let Some(group) = group {
        label.push(' ');
        label.push_str(group);
    }
    label.push(' ');
    label.push_str(&op.name);

    write_stdout_line(&label)?;
    write_stdout_line(&format!("  method: {}", op.method))?;
    write_stdout_line(&format!("  path: {}", op.path))?;
    if let Some(desc) = &op.description {
        write_stdout_line(&format!("  description: {}", desc))?;
    }
    if let Some(defaults) = &op.body_defaults
        && !defaults.is_null()
    {
        write_stdout_line(&format!("  body defaults: {}", defaults))?;
    }
    if !op.args.is_empty() {
        write_stdout_line("  args:")?;
        for arg in &op.args {
            let required = if arg.required { " (required)" } else { "" };
            write_stdout_line(&format!(
                "    --{}  [{}{}]",
                arg.flag, arg.value_type, required
            ))?;
        }
    }
    Ok(())
}

fn handle_tree(tree: &CommandTree, matches: &ArgMatches) -> Result<()> {
    if matches.get_flag("json") {
        return write_output(&serde_json::to_value(tree)?, true);
    }
    write_stdout_line("Run with --json for machine-readable output.")?;
    Ok(())
}

fn handle_auth_doctor(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    timeout: u64,
    pretty: bool,
    matches: &ArgMatches,
) -> Result<()> {
    let project_id = matches
        .get_one::<String>("project_id")
        .ok_or_else(|| anyhow!("project id required"))?;
    let range = matches
        .get_one::<String>("range")
        .map(String::as_str)
        .unwrap_or("today");

    let client = Client::builder()
        .user_agent("openpanel-cli")
        .timeout(Duration::from_secs(timeout))
        .build()
        .context("build http client")?;

    let projects = auth_probe(
        &client,
        base_url,
        client_id,
        client_secret,
        "manage_projects",
        "manage/projects",
        &[],
    )?;
    let clients = auth_probe(
        &client,
        base_url,
        client_id,
        client_secret,
        "manage_clients",
        "manage/clients",
        &[("projectId", project_id.as_str())],
    )?;
    let referrer_name = auth_probe(
        &client,
        base_url,
        client_id,
        client_secret,
        "insights_referrer_name",
        &format!("insights/{project_id}/referrer_name"),
        &[("range", range)],
    )?;

    let manage_ok = projects.success && clients.success;
    let insights_ok = referrer_name.success;
    let classification = if manage_ok {
        "root"
    } else if insights_ok {
        "read"
    } else {
        "write_or_invalid"
    };
    let explanation = match classification {
        "root" => {
            "Root client: Manage and Insights probes succeeded. This should be org/account-wide."
        }
        "read" => {
            "Read client: Insights succeeded but Manage failed. Good for analytics, not org management."
        }
        _ => {
            "Write-only or invalid client: official analytics reads failed. Frontend tracking clients usually land here."
        }
    };

    let output = json!({
        "classification": classification,
        "explanation": explanation,
        "apiUrl": base_url,
        "projectId": project_id,
        "probes": {
            "manageProjects": projects.to_json(),
            "manageClients": clients.to_json(),
            "insightsReferrerName": referrer_name.to_json(),
        }
    });
    write_output(&output, pretty)
}

struct AuthProbe {
    name: String,
    status: u16,
    success: bool,
    body: Value,
}

impl AuthProbe {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "status": self.status,
            "success": self.success,
            "body": redact_secrets(self.body.clone()),
        })
    }
}

fn redact_secrets(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let redacted = if is_secret_key(&key) {
                        Value::String("<redacted>".to_string())
                    } else {
                        redact_secrets(value)
                    };
                    (key, redacted)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_secrets).collect()),
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("secret") || key.contains("token") || key == "password"
}

fn auth_probe(
    client: &Client,
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    name: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<AuthProbe> {
    let mut url = Url::parse(base_url)?.join(path)?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    let resp = client
        .get(url)
        .header("openpanel-client-id", client_id)
        .header("openpanel-client-secret", client_secret)
        .header("accept", "application/json")
        .send()
        .with_context(|| format!("send {name} probe"))?;
    let status = resp.status();
    let body = resp.json().unwrap_or_else(|_| Value::Null);

    Ok(AuthProbe {
        name: name.to_string(),
        status: status.as_u16(),
        success: status.is_success(),
        body,
    })
}

fn auth_error_hint(resource_name: &str, status: StatusCode) -> &'static str {
    if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
        return "";
    }

    match resource_name {
        "export" | "insights" => {
            " — Export/Insights require an OpenPanel read or root client. Frontend/write clients can track events but cannot read analytics."
        }
        "manage" => {
            " — Manage endpoints require an OpenPanel root client with organization-wide access."
        }
        _ => "",
    }
}

fn build_arg(arg: &ArgDef) -> Arg {
    let mut arg_def = Arg::new(arg.name.clone())
        .long(arg.flag.clone())
        .value_name(arg.value_type.clone());

    if arg.list {
        arg_def = arg_def.action(ArgAction::Append);
    }

    if arg.required {
        arg_def = arg_def.required(true);
    }

    arg_def
}

fn find_op<'a>(resource: &'a Resource, group: Option<&str>, op: &str) -> Option<&'a Operation> {
    if resource.groups.is_empty() {
        return resource.ops.iter().find(|op_def| op_def.name == op);
    }

    let group = group?;
    resource
        .groups
        .iter()
        .find(|grp| grp.name == group)
        .and_then(|grp| grp.ops.iter().find(|op_def| op_def.name == op))
}

fn build_request(
    op: &Operation,
    base_url: &str,
    matches: &ArgMatches,
    body_override: Option<&str>,
) -> Result<RequestParts> {
    let mut path = op.path.clone();
    let mut headers = parse_global_headers(matches)?;

    for arg in &op.args {
        if arg.location != "path" {
            continue;
        }
        let value = matches
            .get_one::<String>(&arg.name)
            .ok_or_else(|| anyhow!("missing required argument --{}", arg.flag))?;
        let token = format!("{{{}}}", arg.name);
        path = path.replace(&token, value);
    }

    let base = if op.method == "WS" {
        ws_base_url(base_url)?
    } else {
        base_url.to_string()
    };
    let mut url = Url::parse(&base)?.join(path.trim_start_matches('/'))?;

    add_query_params(&mut url, op, matches)?;
    add_headers(&mut headers, op, matches)?;

    let body = build_body(op, matches, body_override)?;

    Ok((url, headers, body))
}

fn add_query_params(url: &mut Url, op: &Operation, matches: &ArgMatches) -> Result<()> {
    let mut pairs = url.query_pairs_mut();

    for arg in &op.args {
        if arg.location != "query" {
            continue;
        }

        if arg.list {
            if let Some(values) = matches.get_many::<String>(&arg.name) {
                let list: Vec<String> = values.cloned().collect();
                if list.len() == 1 && list[0].trim_start().starts_with('[') {
                    validate_json(&list[0])?;
                    pairs.append_pair(&arg.name, &list[0]);
                } else {
                    for value in list {
                        let rendered = parse_query_value(arg, &value)?;
                        pairs.append_pair(&arg.name, &rendered);
                    }
                }
                continue;
            }
        } else if let Some(value) = matches.get_one::<String>(&arg.name) {
            let rendered = parse_query_value(arg, value)?;
            pairs.append_pair(&arg.name, &rendered);
            continue;
        }

        if arg.required {
            return Err(anyhow!("missing required argument --{}", arg.flag));
        }
    }

    Ok(())
}

fn add_headers(
    headers: &mut Vec<(String, String)>,
    op: &Operation,
    matches: &ArgMatches,
) -> Result<()> {
    for arg in &op.args {
        if arg.location != "header" {
            continue;
        }

        if let Some(value) = matches.get_one::<String>(&arg.name) {
            let header_name = arg.header_name.as_deref().unwrap_or(arg.flag.as_str());
            headers.push((header_name.to_string(), value.to_string()));
        } else if arg.required {
            return Err(anyhow!("missing required argument --{}", arg.flag));
        }
    }
    Ok(())
}

fn build_body(
    op: &Operation,
    matches: &ArgMatches,
    body_override: Option<&str>,
) -> Result<Option<Value>> {
    let method_allows_body = matches!(op.method.as_str(), "POST" | "PATCH" | "PUT");

    if let Some(raw) = body_override {
        if !method_allows_body {
            return Err(anyhow!("--body is only supported for POST/PATCH/PUT"));
        }
        let parsed: Value = serde_json::from_str(raw).context("invalid JSON for --body")?;
        return Ok(Some(parsed));
    }

    let mut body = op
        .body_defaults
        .clone()
        .unwrap_or_else(|| Value::Object(Map::new()));

    let mut has_body = !body.as_object().map(|obj| obj.is_empty()).unwrap_or(false);

    for arg in &op.args {
        if arg.location != "body" {
            continue;
        }

        if arg.list {
            if let Some(values) = matches.get_many::<String>(&arg.name) {
                let list: Vec<String> = values.cloned().collect();
                let parsed = parse_list_value(arg, &list)?;
                let path = arg.path.as_deref().unwrap_or(&arg.name);
                set_json_path(&mut body, path, parsed)?;
                has_body = true;
                continue;
            }
        } else if let Some(value) = matches.get_one::<String>(&arg.name) {
            let parsed = parse_scalar_value(arg, value)?;
            let path = arg.path.as_deref().unwrap_or(&arg.name);
            set_json_path(&mut body, path, parsed)?;
            has_body = true;
            continue;
        }

        if arg.required {
            return Err(anyhow!("missing required argument --{}", arg.flag));
        }
    }

    if has_body {
        if !method_allows_body {
            return Err(anyhow!(
                "body parameters are only supported for POST/PATCH/PUT"
            ));
        }
        Ok(Some(body))
    } else {
        Ok(None)
    }
}

fn parse_query_value(arg: &ArgDef, value: &str) -> Result<String> {
    match arg.value_type.as_str() {
        "bool" => Ok(parse_bool(value)?.to_string()),
        "int" => Ok(value.parse::<i64>().context("invalid integer")?.to_string()),
        "float" => Ok(value.parse::<f64>().context("invalid float")?.to_string()),
        "json" => {
            validate_json(value)?;
            Ok(value.to_string())
        }
        _ => Ok(value.to_string()),
    }
}

fn parse_list_value(arg: &ArgDef, values: &[String]) -> Result<Value> {
    if values.len() == 1 && values[0].trim_start().starts_with('[') {
        let parsed: Value = serde_json::from_str(&values[0]).context("invalid JSON list")?;
        return Ok(parsed);
    }

    let mut out = Vec::new();
    for value in values {
        out.push(parse_scalar_value(arg, value)?);
    }
    Ok(Value::Array(out))
}

fn parse_scalar_value(arg: &ArgDef, value: &str) -> Result<Value> {
    if arg.nullable && value == "null" {
        return Ok(Value::Null);
    }

    match arg.value_type.as_str() {
        "int" => Ok(Value::Number(value.parse::<i64>()?.into())),
        "float" => Ok(json!(value.parse::<f64>()?)),
        "bool" => Ok(Value::Bool(parse_bool(value)?)),
        "json" => {
            let parsed: Value = serde_json::from_str(value).context("invalid JSON value")?;
            Ok(parsed)
        }
        _ => Ok(Value::String(value.to_string())),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(anyhow!("invalid boolean: {value}")),
    }
}

fn validate_json(value: &str) -> Result<()> {
    serde_json::from_str::<Value>(value).context("invalid JSON")?;
    Ok(())
}

fn set_json_path(root: &mut Value, path: &str, value: Value) -> Result<()> {
    if path == "$" {
        *root = value;
        return Ok(());
    }

    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return Err(anyhow!("invalid body path"));
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        let obj = current.as_object_mut().context("body root is not object")?;
        current = obj
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }

    if !current.is_object() {
        *current = Value::Object(Map::new());
    }
    let obj = current.as_object_mut().context("body root is not object")?;
    obj.insert(segments[segments.len() - 1].to_string(), value);
    Ok(())
}

#[derive(Clone, Debug)]
struct EventQuery {
    project_id: String,
    events: Vec<String>,
    start: String,
    end: String,
    filters: Vec<Filter>,
    select: Vec<String>,
    format: String,
    output: Option<String>,
    limit: usize,
    max_pages: Option<usize>,
    debug: bool,
}

#[derive(Clone, Debug)]
enum FilterOp {
    Eq,
    Ne,
}

#[derive(Clone, Debug)]
struct Filter {
    path: String,
    op: FilterOp,
    value: String,
}

#[derive(Default, Debug, Clone)]
struct FetchStats {
    pages: usize,
    api_rows: usize,
    time_rows: usize,
    matched_rows: usize,
}

struct FetchContext<'a> {
    client: &'a Client,
    base_url: &'a str,
    client_id: &'a str,
    client_secret: &'a str,
}

fn handle_events_query(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    timeout: u64,
    matches: &ArgMatches,
) -> Result<()> {
    let query = parse_event_query(matches, None)?;
    let client = analytics_client(timeout)?;
    let (rows, stats) = fetch_events(&client, base_url, client_id, client_secret, &query)?;
    if query.debug {
        eprintln!(
            "debug: pages={} api_rows={} after_time={} matched={}",
            stats.pages, stats.api_rows, stats.time_rows, stats.matched_rows
        );
    }
    write_rows(&rows, &query)
}

fn handle_funnel(
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    timeout: u64,
    matches: &ArgMatches,
) -> Result<()> {
    let steps = split_csv_required(matches, "steps")?;
    let unit = matches
        .get_one::<String>("unit")
        .cloned()
        .unwrap_or_else(|| "sessionId".to_string());
    let select = format!("createdAt,name,{unit}");
    let query = parse_event_query(matches, Some((&steps, &select, "json")))?;
    let client = analytics_client(timeout)?;
    let (rows, stats) = fetch_events(&client, base_url, client_id, client_secret, &query)?;
    let result = build_funnel(&rows, &steps, &unit);
    let format = matches
        .get_one::<String>("format")
        .map(String::as_str)
        .unwrap_or("table");

    if query.debug {
        eprintln!(
            "debug: pages={} api_rows={} after_time={} matched={}",
            stats.pages, stats.api_rows, stats.time_rows, stats.matched_rows
        );
    }

    if format == "json" {
        write_output(
            &json!({"steps": result, "stats": {
                "pages": stats.pages,
                "apiRows": stats.api_rows,
                "timeRows": stats.time_rows,
                "matchedRows": stats.matched_rows,
            }}),
            true,
        )
    } else {
        write_stdout_line("step\tusers\tconversion\tstep_conversion")?;
        let first = result
            .first()
            .and_then(|v| v.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut prev = first;
        for item in result {
            let step = item.get("step").and_then(Value::as_str).unwrap_or("");
            let count = item.get("count").and_then(Value::as_u64).unwrap_or(0);
            let conversion = pct(count, first);
            let step_conversion = pct(count, prev);
            write_stdout_line(&format!("{step}\t{count}\t{conversion}\t{step_conversion}"))?;
            prev = count;
        }
        Ok(())
    }
}

fn parse_event_query(
    matches: &ArgMatches,
    funnel_defaults: Option<(&[String], &str, &str)>,
) -> Result<EventQuery> {
    let project_id = matches
        .get_one::<String>("project_id")
        .cloned()
        .ok_or_else(|| anyhow!("--project-id required"))?;
    let events = if let Some((steps, _, _)) = funnel_defaults {
        steps.to_vec()
    } else {
        split_csv_required(matches, "events")?
    };
    let (start, end) = parse_time_window(matches)?;
    let filters = parse_where(matches.get_one::<String>("where").map(String::as_str))?;
    let select = if let Some((_, select, _)) = funnel_defaults {
        split_csv(select)
    } else {
        matches
            .get_one::<String>("select")
            .map(|s| split_csv(s))
            .unwrap_or_default()
    };
    let format = funnel_defaults
        .map(|(_, _, f)| f.to_string())
        .or_else(|| matches.get_one::<String>("format").cloned())
        .unwrap_or_else(|| "jsonl".to_string());
    Ok(EventQuery {
        project_id,
        events,
        start,
        end,
        filters,
        select,
        format,
        output: matches
            .try_get_one::<String>("output")
            .ok()
            .flatten()
            .cloned(),
        limit: matches
            .get_one::<usize>("limit")
            .copied()
            .unwrap_or(1000)
            .clamp(1, 1000),
        max_pages: matches.get_one::<usize>("max_pages").copied(),
        debug: matches.get_flag("debug"),
    })
}

fn analytics_client(timeout: u64) -> Result<Client> {
    Client::builder()
        .user_agent("openpanel-cli")
        .timeout(Duration::from_secs(timeout))
        .build()
        .context("build http client")
}

fn fetch_events(
    client: &Client,
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    query: &EventQuery,
) -> Result<(Vec<Value>, FetchStats)> {
    let mut out = Vec::new();
    let mut stats = FetchStats::default();
    let includes_properties = needs_properties(query);

    for event in &query.events {
        let mut page = 1usize;
        loop {
            if query.max_pages.is_some_and(|max| page > max) {
                break;
            }
            let ctx = FetchContext {
                client,
                base_url,
                client_id,
                client_secret,
            };
            let page_value = fetch_events_page(&ctx, query, event, page, includes_properties)?;
            stats.pages += 1;
            let data = page_value
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if data.is_empty() {
                break;
            }
            stats.api_rows += data.len();
            let mut older_than_start = 0usize;
            for row in data {
                let created = row
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if created < query.start.as_str() {
                    older_than_start += 1;
                    continue;
                }
                if created > query.end.as_str() {
                    continue;
                }
                stats.time_rows += 1;
                if !matches_filters(&row, &query.filters) {
                    continue;
                }
                stats.matched_rows += 1;
                out.push(project_row(&row, &query.select)?);
            }
            if older_than_start > 0 {
                break;
            }
            let pages = page_value
                .pointer("/meta/pages")
                .and_then(Value::as_u64)
                .unwrap_or(page as u64);
            if page as u64 >= pages {
                break;
            }
            page += 1;
        }
    }

    out.sort_by(|a, b| {
        let at = a.get("createdAt").and_then(Value::as_str).unwrap_or("");
        let bt = b.get("createdAt").and_then(Value::as_str).unwrap_or("");
        at.cmp(bt)
    });
    Ok((out, stats))
}

fn fetch_events_page(
    ctx: &FetchContext<'_>,
    query: &EventQuery,
    event: &str,
    page: usize,
    includes_properties: bool,
) -> Result<Value> {
    let mut url = Url::parse(ctx.base_url)?.join("export/events")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("projectId", &query.project_id);
        pairs.append_pair("event", event);
        pairs.append_pair("start", &query.start);
        pairs.append_pair("end", &query.end);
        pairs.append_pair("page", &page.to_string());
        pairs.append_pair("limit", &query.limit.to_string());
        if includes_properties {
            pairs.append_pair("includes", "properties");
        }
    }

    let mut last_err = None;
    for attempt in 0..3 {
        let resp = ctx
            .client
            .get(url.clone())
            .header("openpanel-client-id", ctx.client_id)
            .header("openpanel-client-secret", ctx.client_secret)
            .header("accept", "application/json")
            .send();
        match resp {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if status.is_success() {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(value) => return Ok(value),
                        Err(err) => {
                            last_err = Some(anyhow!("decode json: {err}; body={}", truncate(&text)))
                        }
                    }
                } else if retryable_status(status) {
                    last_err = Some(anyhow!("http {status}; body={}", truncate(&text)));
                } else {
                    return Err(anyhow!("http {status}; body={}", truncate(&text)));
                }
            }
            Err(err) => last_err = Some(anyhow!("send request: {err}")),
        }
        sleep(Duration::from_millis(250 * (attempt + 1) as u64));
    }
    Err(last_err.unwrap_or_else(|| anyhow!("request failed")))
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn write_rows(rows: &[Value], query: &EventQuery) -> Result<()> {
    let mut writer: Box<dyn Write> = if let Some(path) = &query.output {
        Box::new(File::create(path).with_context(|| format!("create {path}"))?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    match query.format.as_str() {
        "json" => writeln!(writer, "{}", serde_json::to_string(rows)?)?,
        "jsonl" => {
            for row in rows {
                writeln!(writer, "{}", serde_json::to_string(row)?)?;
            }
        }
        "csv" => write_csv(&mut writer, rows, &query.select)?,
        "table" => write_table(&mut writer, rows, &query.select)?,
        _ => return Err(anyhow!("unsupported format {}", query.format)),
    }
    Ok(())
}

fn write_csv(writer: &mut dyn Write, rows: &[Value], select: &[String]) -> Result<()> {
    let fields = output_fields(rows, select);
    writeln!(writer, "{}", fields.join(","))?;
    for row in rows {
        let cells: Vec<String> = fields
            .iter()
            .map(|field| csv_escape(value_to_cell(get_path(row, field))))
            .collect();
        writeln!(writer, "{}", cells.join(","))?;
    }
    Ok(())
}

fn write_table(writer: &mut dyn Write, rows: &[Value], select: &[String]) -> Result<()> {
    let fields = output_fields(rows, select);
    writeln!(writer, "{}", fields.join("\t"))?;
    for row in rows {
        let cells: Vec<String> = fields
            .iter()
            .map(|field| value_to_cell(get_path(row, field)))
            .collect();
        writeln!(writer, "{}", cells.join("\t"))?;
    }
    Ok(())
}

fn output_fields(rows: &[Value], select: &[String]) -> Vec<String> {
    if !select.is_empty() {
        return select.to_vec();
    }
    rows.first()
        .and_then(Value::as_object)
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

fn project_row(row: &Value, select: &[String]) -> Result<Value> {
    if select.is_empty() {
        return Ok(row.clone());
    }
    let mut out = Value::Object(Map::new());
    for field in select {
        let value = get_path(row, field).cloned().unwrap_or(Value::Null);
        set_json_path(&mut out, field, value)?;
    }
    Ok(out)
}

fn needs_properties(query: &EventQuery) -> bool {
    query.select.iter().any(|s| s.starts_with("properties."))
        || query
            .filters
            .iter()
            .any(|filter| filter.path.starts_with("properties."))
}

fn matches_filters(row: &Value, filters: &[Filter]) -> bool {
    filters.iter().all(|filter| {
        let actual = value_to_cell(get_path(row, &filter.path));
        match filter.op {
            FilterOp::Eq => actual == filter.value,
            FilterOp::Ne => actual != filter.value,
        }
    })
}

fn parse_where(raw: Option<&str>) -> Result<Vec<Filter>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(" and ")
        .map(|part| {
            let part = part.trim();
            if let Some((path, value)) = part.split_once("!=") {
                return Ok(Filter {
                    path: path.trim().to_string(),
                    op: FilterOp::Ne,
                    value: unquote(value.trim()),
                });
            }
            if let Some((path, value)) = part.split_once('=') {
                return Ok(Filter {
                    path: path.trim().to_string(),
                    op: FilterOp::Eq,
                    value: unquote(value.trim()),
                });
            }
            Err(anyhow!("invalid --where clause: {part}"))
        })
        .collect()
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn value_to_cell(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(value) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn csv_escape(value: String) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

fn build_funnel(rows: &[Value], steps: &[String], unit: &str) -> Vec<Value> {
    let step_index: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(idx, step)| (step.as_str(), idx))
        .collect();
    let mut progress: HashMap<String, usize> = HashMap::new();
    let mut counts: Vec<HashSet<String>> = (0..steps.len()).map(|_| HashSet::new()).collect();

    for row in rows {
        let Some(name) = row.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(&idx) = step_index.get(name) else {
            continue;
        };
        let unit_value = value_to_cell(get_path(row, unit));
        if unit_value.is_empty() {
            continue;
        }
        let current = *progress.get(&unit_value).unwrap_or(&0);
        if idx == current {
            counts[idx].insert(unit_value.clone());
            progress.insert(unit_value, current + 1);
        }
    }

    steps
        .iter()
        .enumerate()
        .map(|(idx, step)| json!({"step": step, "count": counts[idx].len()}))
        .collect()
}

fn pct(count: u64, total: u64) -> String {
    if total == 0 {
        return "0.00%".to_string();
    }
    format!("{:.2}%", (count as f64 / total as f64) * 100.0)
}

fn parse_time_window(matches: &ArgMatches) -> Result<(String, String)> {
    if let Some(day) = matches.get_one::<String>("day") {
        let offset = matches
            .get_one::<String>("tz_offset")
            .map(String::as_str)
            .unwrap_or("+00:00");
        return local_day_window(day, offset);
    }
    let start = matches
        .get_one::<String>("start")
        .cloned()
        .ok_or_else(|| anyhow!("--start or --day required"))?;
    let end = matches
        .get_one::<String>("end")
        .cloned()
        .ok_or_else(|| anyhow!("--end or --day required"))?;
    Ok((
        normalize_rfc3339_bound(&start, false),
        normalize_rfc3339_bound(&end, true),
    ))
}

fn local_day_window(day: &str, offset: &str) -> Result<(String, String)> {
    let (year, month, date) = parse_day(day)?;
    let offset_minutes = parse_offset_minutes(offset)?;
    let start_epoch = days_from_civil(year, month, date) * 86_400 - (offset_minutes as i64 * 60);
    let end_epoch = start_epoch + 86_400 - 1;
    Ok((format_epoch(start_epoch, 0), format_epoch(end_epoch, 999)))
}

fn parse_day(day: &str) -> Result<(i32, u32, u32)> {
    let mut parts = day.split('-');
    let year = parts.next().context("invalid --day")?.parse()?;
    let month = parts.next().context("invalid --day")?.parse()?;
    let date = parts.next().context("invalid --day")?.parse()?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid --day"));
    }
    Ok((year, month, date))
}

fn parse_offset_minutes(offset: &str) -> Result<i32> {
    let sign = match &offset[..1] {
        "+" => 1,
        "-" => -1,
        _ => return Err(anyhow!("invalid --tz-offset")),
    };
    let mut parts = offset[1..].split(':');
    let hours: i32 = parts.next().context("invalid --tz-offset")?.parse()?;
    let minutes: i32 = parts.next().context("invalid --tz-offset")?.parse()?;
    Ok(sign * (hours * 60 + minutes))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - (month <= 2) as i32;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) as i64
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + (month <= 2) as i32, month as u32, day as u32)
}

fn format_epoch(epoch: i64, millis: u16) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs / 3600;
    let minute = (secs % 3600) / 60;
    let second = secs % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn normalize_rfc3339_bound(value: &str, is_end: bool) -> String {
    if !value.ends_with('Z') || value.contains('.') {
        return value.to_string();
    }
    let millis = if is_end { "999" } else { "000" };
    format!("{}.{millis}Z", value.trim_end_matches('Z'))
}

fn split_csv_required(matches: &ArgMatches, name: &str) -> Result<Vec<String>> {
    let raw = matches
        .get_one::<String>(name)
        .ok_or_else(|| anyhow!("--{} required", name.replace('_', "-")))?;
    Ok(split_csv(raw))
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn truncate(text: &str) -> String {
    const MAX: usize = 500;
    if text.len() > MAX {
        format!("{}…", &text[..MAX])
    } else {
        text.to_string()
    }
}

fn write_output(value: &Value, pretty: bool) -> Result<()> {
    if pretty {
        write_stdout_line(&serde_json::to_string_pretty(value)?)
    } else {
        write_stdout_line(&serde_json::to_string(value)?)
    }
}

fn parse_global_headers(matches: &ArgMatches) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    if let Some(values) = matches.get_many::<String>("header") {
        for raw in values {
            let (key, value) = parse_header(raw)?;
            headers.push((key, value));
        }
    }
    Ok(headers)
}

fn parse_header(raw: &str) -> Result<(String, String)> {
    if let Some((key, value)) = raw.split_once('=') {
        return Ok((key.trim().to_string(), value.trim().to_string()));
    }
    if let Some((key, value)) = raw.split_once(':') {
        return Ok((key.trim().to_string(), value.trim().to_string()));
    }
    Err(anyhow!(
        "invalid header format, expected KEY=VALUE or KEY:VALUE"
    ))
}

fn ws_base_url(base_url: &str) -> Result<String> {
    let mut url = Url::parse(base_url)?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| anyhow!("invalid ws scheme"))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| anyhow!("invalid ws scheme"))?,
        "wss" | "ws" => {}
        _ => return Err(anyhow!("unsupported ws base url scheme")),
    }
    Ok(url.to_string())
}

fn execute_websocket(url: Url, headers: Vec<(String, String)>) -> Result<()> {
    let mut request = url.as_str().into_client_request()?;
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes())?;
        let value = HeaderValue::from_str(&value)?;
        request.headers_mut().append(name, value);
    }

    let (mut socket, _response) = connect(request)?;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                write_stdout_line(&text)?;
            }
            Ok(Message::Binary(bin)) => {
                write_stdout_line(&format!("binary: {} bytes", bin.len()))?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

fn write_stdout_line(value: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if let Err(err) = out.write_all(value.as_bytes()) {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        return Err(err.into());
    }
    if let Err(err) = out.write_all(b"\n") {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        return Err(err.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_day_window() {
        let (start, end) = local_day_window("2026-05-12", "+07:00").unwrap();
        assert_eq!(start, "2026-05-11T17:00:00.000Z");
        assert_eq!(end, "2026-05-12T16:59:59.999Z");
    }

    #[test]
    fn parses_where_filters() {
        let filters = parse_where(Some(
            "country=US and properties.utm_source='meta' and os!=Android",
        ))
        .unwrap();
        assert_eq!(filters.len(), 3);
        assert!(matches!(filters[0].op, FilterOp::Eq));
        assert_eq!(filters[1].path, "properties.utm_source");
        assert_eq!(filters[1].value, "meta");
        assert!(matches!(filters[2].op, FilterOp::Ne));
    }

    #[test]
    fn projects_dotted_fields() {
        let row = json!({
            "createdAt": "2026-05-12T00:00:00.000Z",
            "name": "screen_view",
            "properties": {"utm_source": "meta", "utm_content": "ad"}
        });
        let projected =
            project_row(&row, &split_csv("createdAt,name,properties.utm_source")).unwrap();
        assert_eq!(projected["properties"]["utm_source"], "meta");
        assert!(projected["properties"].get("utm_content").is_none());
    }

    #[test]
    fn matches_nested_filters() {
        let row = json!({
            "country": "US",
            "properties": {"utm_source": "meta"}
        });
        let filters = parse_where(Some("country=US and properties.utm_source=meta")).unwrap();
        assert!(matches_filters(&row, &filters));
    }

    #[test]
    fn builds_ordered_funnel() {
        let rows = vec![
            json!({"name": "screen_view", "sessionId": "s1"}),
            json!({"name": "quiz_started", "sessionId": "s1"}),
            json!({"name": "quiz_started", "sessionId": "s2"}),
            json!({"name": "paywall_viewed", "sessionId": "s1"}),
        ];
        let steps = split_csv("screen_view,quiz_started,paywall_viewed");
        let result = build_funnel(&rows, &steps, "sessionId");
        assert_eq!(result[0]["count"], 1);
        assert_eq!(result[1]["count"], 1);
        assert_eq!(result[2]["count"], 1);
    }
}
