mod command_tree;

use anyhow::{Context, Result, anyhow};
use clap::{Arg, ArgAction, ArgMatches, Command};
use command_tree::{ArgDef, CommandTree, Operation, Resource};
use reqwest::blocking::Client;
use reqwest::Url;
use serde_json::{Map, Value, json};
use std::env;
use std::io::Write;
use std::time::Duration;
use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::connect;
use tungstenite::http::{HeaderName, HeaderValue};

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

    let group_label = group_name.map(|name| format!("{name} ")).unwrap_or_default();
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
        return Err(anyhow!("http {}", status));
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
        Command::new("tree")
            .about("Show full command tree")
            .arg(
                Arg::new("json")
                    .long("json")
                    .action(ArgAction::SetTrue)
                    .help("Emit machine-readable JSON"),
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
                res_cmd = res_cmd.subcommand(op_cmd);
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

    cmd
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
                        let ops: Vec<String> =
                            group.ops.iter().map(|op| op.name.clone()).collect();
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
    if let Some(defaults) = &op.body_defaults {
        if !defaults.is_null() {
            write_stdout_line(&format!("  body defaults: {}", defaults))?;
        }
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
) -> Result<(Url, Vec<(String, String)>, Option<Value>)> {
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
            let header_name = arg
                .header_name
                .as_deref()
                .unwrap_or_else(|| arg.flag.as_str());
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
            return Err(anyhow!("body parameters are only supported for POST/PATCH/PUT"));
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
    Err(anyhow!("invalid header format, expected KEY=VALUE or KEY:VALUE"))
}

fn ws_base_url(base_url: &str) -> Result<String> {
    let mut url = Url::parse(base_url)?;
    match url.scheme() {
        "https" => url.set_scheme("wss").map_err(|_| anyhow!("invalid ws scheme"))?,
        "http" => url.set_scheme("ws").map_err(|_| anyhow!("invalid ws scheme"))?,
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
