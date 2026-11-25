//! The plan limits of a Claude Code account: the five-hour and weekly windows a subscription
//! has, or that the account pays by API key. Claude Code answers a `get_usage` control
//! request on stream-json input without a model call; standard input closing ends it.

use serde_json::{Value, json};

/// Claude Code's arguments for the request. `--setting-sources project` in an empty folder
/// runs none of the user's hooks; the login stays (`--bare` would drop it).
pub const CLAUDE_ARGS: [&str; 10] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--setting-sources",
    "project",
];

/// The program and arguments that ask: on this computer the program found on PATH (an npm
/// shim runs its script with node), run in a folder of the caller's; on a host a login
/// shell, whose PATH has the harness, in [`HOST_FOLDER`].
pub fn claude_command(local: bool) -> (String, Vec<String>) {
    let args = CLAUDE_ARGS.iter().map(|arg| (*arg).to_owned());
    if local {
        let (program, prefix) = crate::resolve_program("claude").unwrap_or_else(|| ("claude".into(), Vec::new()));
        (program.to_string_lossy().into_owned(), prefix.into_iter().chain(args).collect())
    } else {
        // The arguments are plain words: no quoting needed inside the login shell.
        let line = std::iter::once("claude".to_owned()).chain(args).collect::<Vec<_>>().join(" ");
        let script = format!("umask 077; d=\"$HOME/{HOST_FOLDER}\"; mkdir -p \"$d\" && cd \"$d\" && exec \"${{SHELL:-sh}}\" -lc \"$1\"");
        ("sh".into(), vec!["-c".into(), script, "sh".into(), line])
    }
}

/// The folder the request runs in on a host, under the home folder: the user's own and
/// empty. A folder others may write to (`/tmp`) could hold project settings with a hook
/// that would then run as the user.
pub const HOST_FOLDER: &str = ".cache/bowyer/usage";

/// The lines to send on standard input: the plan, and the status that tells whether anyone
/// is signed in (without a login the plan reads like an API key's).
pub fn claude_request() -> String {
    [
        json!({"type": "control_request", "request_id": USAGE, "request": {"subtype": "get_usage", "skip_behaviors": true}}),
        json!({"type": "control_request", "request_id": STATUS, "request": {"subtype": "get_status"}}),
    ]
    .iter()
    .map(|line| format!("{line}\n"))
    .collect()
}

const USAGE: &str = "bowyer-usage";
const STATUS: &str = "bowyer-status";

/// The answer to the request `id`, or to the only request of an older reader.
fn response_to<'a>(events: &'a [Value], id: &str) -> Option<&'a Value> {
    let responses = || events.iter().filter(|event| event.get("type").and_then(Value::as_str) == Some("control_response"));
    responses()
        .find(|event| event.pointer("/response/request_id").and_then(Value::as_str) == Some(id))
        .or_else(|| (id == USAGE).then(|| responses().next()).flatten())
        .and_then(|event| event.get("response"))
}

/// Whether the status names a way of signing in: a login method, an API key, a provider.
/// `None` when the status did not come (an older Claude Code).
fn signed_in(status: Option<&Value>) -> Option<bool> {
    let status = status.filter(|status| status.get("subtype").and_then(Value::as_str) == Some("success"))?;
    let sections = status.pointer("/response/sections")?.as_array()?;
    let labels = sections.iter().filter_map(|section| section.get("rows")?.as_array()).flatten().filter_map(|row| row.get("label")?.as_str());
    let words = ["login", "auth", "api key", "account", "organization", "email", "provider", "token"];
    Some(labels.map(str::to_ascii_lowercase).any(|label| words.iter().any(|word| label.contains(word))))
}

/// Who Claude Code runs as on a machine, as its status says (`get_status`): the way it is
/// signed in ("Claude Max account", an API key) and the account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub method: Option<String>,
    pub email: Option<String>,
    pub organization: Option<String>,
}

/// Reads who Claude Code runs as from its output for [`claude_request`]; `None` without a
/// status (an older Claude Code) or with nothing in it about the login. A row about an API
/// key gives only its label: its value may hold part of the key.
pub fn parse_account(output: &str) -> Option<Account> {
    let events: Vec<Value> = output.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok()).collect();
    let status = response_to(&events, STATUS).filter(|status| status.get("subtype").and_then(Value::as_str) == Some("success"))?;
    let sections = status.pointer("/response/sections")?.as_array()?;
    let mut account = Account::default();
    for row in sections.iter().filter_map(|section| section.get("rows")?.as_array()).flatten() {
        let (Some(label), value) = (row.get("label").and_then(Value::as_str), row.get("value").and_then(Value::as_str)) else { continue };
        let value = value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned);
        match label.to_ascii_lowercase().as_str() {
            "login method" | "auth method" | "authentication" => account.method = account.method.take().or(value),
            "email" => account.email = value,
            "organization" => account.organization = value,
            other if other.contains("api key") || other.contains("auth token") => account.method = Some(label.to_owned()),
            _ => {}
        }
    }
    (account != Account::default()).then_some(account)
}

/// What an account's plan is.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// A subscription with its windows; `name` is the plan ("max", "pro").
    Limits { name: Option<String>, windows: Vec<Window>, extra: Option<Spend> },
    /// Plan limits do not apply: an API key or a cloud provider.
    Api,
    /// The harness did not say: an older version, no login, an error.
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    FiveHours,
    Week,
}

/// One window of a plan's limit.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub kind: WindowKind,
    /// The model a weekly window holds to alone, as the server names it.
    pub model: Option<String>,
    /// The share used, 0-100.
    pub percent: f64,
    pub resets_ms: Option<i64>,
}

/// Money spent past the plan's limits this month.
#[derive(Clone, Debug, PartialEq)]
pub struct Spend {
    pub used: f64,
    pub limit: Option<f64>,
    pub currency: String,
}

/// Reads Claude Code's output for [`claude_request`].
pub fn parse_claude(output: &str) -> Plan {
    let events: Vec<Value> = output.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok()).collect();
    let Some(response) = response_to(&events, USAGE) else {
        return Plan::Unknown(model::shorten(output.trim(), 200));
    };
    if response.get("subtype").and_then(Value::as_str) != Some("success") {
        let error = response.get("error").and_then(Value::as_str).unwrap_or("error");
        return Plan::Unknown(error.to_owned());
    }
    let body = response.get("response").cloned().unwrap_or(Value::Null);
    if body.get("rate_limits_available").and_then(Value::as_bool) == Some(false) {
        // Without limits and without a subscription the account pays by API; a subscriber
        // can lack them too (a login without the profile scope).
        return match body.get("subscription_type") {
            Some(Value::String(_)) => Plan::Unknown("rate limits unavailable".into()),
            // Nobody signed in reads like an API key: the status tells them apart.
            _ if signed_in(response_to(&events, STATUS)) == Some(false) => Plan::Unknown("Claude Code is not signed in".into()),
            _ => Plan::Api,
        };
    }
    let Some(limits) = body.get("rate_limits").filter(|limits| limits.is_object()) else {
        return Plan::Unknown("no rate limits".into());
    };
    let name = body.get("subscription_type").and_then(Value::as_str).map(str::to_owned);
    let mut windows = rows(limits);
    if windows.is_empty() {
        windows = named_windows(limits);
    }
    Plan::Limits { name, windows, extra: spend(limits) }
}

/// The rows of `limits[]`, classified by their kind.
fn rows(limits: &Value) -> Vec<Window> {
    let Some(rows) = limits.get("limits").and_then(Value::as_array) else { return Vec::new() };
    rows.iter()
        .filter_map(|row| {
            let (kind, model) = match row.get("kind").and_then(Value::as_str)? {
                "session" => (WindowKind::FiveHours, None),
                "weekly_all" => (WindowKind::Week, None),
                "weekly_scoped" => {
                    let model = row.pointer("/scope/model/display_name").and_then(Value::as_str)?;
                    (WindowKind::Week, Some(model.to_owned()))
                }
                _ => return None,
            };
            Some(Window { kind, model, percent: row.get("percent").and_then(Value::as_f64)?, resets_ms: resets(row) })
        })
        .collect()
}

/// The windows by name, for answers without `limits[]`.
fn named_windows(limits: &Value) -> Vec<Window> {
    [("five_hour", WindowKind::FiveHours), ("seven_day", WindowKind::Week)]
        .into_iter()
        .filter_map(|(name, kind)| {
            let window = limits.get(name)?;
            Some(Window { kind, model: None, percent: window.get("utilization").and_then(Value::as_f64)?, resets_ms: resets(window) })
        })
        .collect()
}

fn resets(window: &Value) -> Option<i64> {
    window.get("resets_at").and_then(Value::as_str).and_then(history::parse_rfc3339_ms)
}

/// Spending past the limits, when the account has it on.
fn spend(limits: &Value) -> Option<Spend> {
    let spend = limits.get("spend")?;
    if spend.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let money = |value: &Value| -> Option<(f64, String)> {
        let minor = value.get("amount_minor").and_then(Value::as_f64)?;
        let exponent = value.get("exponent").and_then(Value::as_i64).unwrap_or(2) as i32;
        let currency = value.get("currency").and_then(Value::as_str).unwrap_or("USD").to_owned();
        Some((minor / 10f64.powi(exponent), currency))
    };
    let (used, currency) = money(spend.get("used")?)?;
    let limit = spend.get("limit").and_then(money).map(|(limit, _)| limit);
    Some(Spend { used, limit, currency })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(body: Value) -> String {
        let events = [
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "bowyer-usage", "response": body}}),
        ];
        events.iter().map(|event| format!("{event}\n")).collect()
    }

    #[test]
    fn a_subscription_reads_as_its_windows() {
        let output = answer(json!({
            "subscription_type": "pro",
            "rate_limits_available": true,
            "rate_limits": {
                "five_hour": {"utilization": 9, "resets_at": "2030-01-01T05:00:00.000000+00:00"},
                "seven_day": {"utilization": 40, "resets_at": "2030-01-07T00:00:00+00:00"},
                "limits": [
                    {"kind": "session", "group": "session", "percent": 12, "resets_at": "2030-01-01T05:00:00.123456+00:00"},
                    {"kind": "weekly_all", "group": "weekly", "percent": 41, "resets_at": "2030-01-07T00:00:00Z"},
                    {"kind": "weekly_scoped", "group": "weekly", "percent": 5, "resets_at": null, "scope": {"model": {"display_name": "Bigmodel"}}},
                    {"kind": "something_new", "percent": 1}
                ],
                "spend": {"enabled": true, "used": {"amount_minor": 1234, "currency": "USD", "exponent": 2}, "limit": {"amount_minor": 5000, "currency": "USD", "exponent": 2}}
            }
        }));
        let Plan::Limits { name, windows, extra } = parse_claude(&output) else { panic!("limits") };
        assert_eq!(name.as_deref(), Some("pro"));
        assert_eq!(windows.len(), 3, "the rows win over the named windows; unknown kinds stay out");
        assert_eq!((windows[0].kind, windows[0].percent), (WindowKind::FiveHours, 12.0));
        assert_eq!(windows[0].resets_ms, history::parse_rfc3339_ms("2030-01-01T05:00:00.123Z"));
        assert_eq!((windows[2].kind, windows[2].model.as_deref()), (WindowKind::Week, Some("Bigmodel")));
        assert_eq!(windows[2].resets_ms, None);
        assert_eq!(extra, Some(Spend { used: 12.34, limit: Some(50.0), currency: "USD".into() }));
    }

    #[test]
    fn an_answer_without_rows_uses_the_named_windows() {
        let output = answer(json!({
            "rate_limits_available": true,
            "rate_limits": {"five_hour": {"utilization": 3, "resets_at": "2030-01-01T05:00:00+00:00"}, "seven_day": null}
        }));
        let Plan::Limits { windows, extra, .. } = parse_claude(&output) else { panic!("limits") };
        assert_eq!(windows, vec![Window { kind: WindowKind::FiveHours, model: None, percent: 3.0, resets_ms: history::parse_rfc3339_ms("2030-01-01T05:00:00Z") }]);
        assert_eq!(extra, None);
    }

    #[test]
    fn an_api_key_and_failures_read_apart() {
        let api = answer(json!({"subscription_type": null, "rate_limits_available": false, "rate_limits": null}));
        assert_eq!(parse_claude(&api), Plan::Api, "an older Claude Code without the status");
        let status = |labels: &[&str]| {
            let rows: Vec<Value> = labels.iter().map(|label| json!({"label": label, "value": "x"})).collect();
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "bowyer-status", "response": {"sections": [{"title": "Session", "rows": rows}]}}}).to_string()
        };
        let keyed = format!("{api}{}\n", status(&["Version", "Session ID", "API key"]));
        assert_eq!(parse_claude(&keyed), Plan::Api);
        let nobody = format!("{api}{}\n", status(&["Version", "Session ID", "Session kind", "Peer address", "cwd"]));
        assert_eq!(parse_claude(&nobody), Plan::Unknown("Claude Code is not signed in".into()), "nobody signed in is no API account");
        let scopeless = answer(json!({"subscription_type": "max", "rate_limits_available": false, "rate_limits": null}));
        assert!(matches!(parse_claude(&scopeless), Plan::Unknown(_)), "a subscriber without limits is no API account");
        let refused = r#"{"type":"control_response","response":{"subtype":"error","request_id":"bowyer-usage","error":"Unsupported control request subtype: get_usage"}}"#;
        assert_eq!(parse_claude(refused), Plan::Unknown("Unsupported control request subtype: get_usage".into()));
        assert!(matches!(parse_claude("Not logged in"), Plan::Unknown(text) if text == "Not logged in"));
    }

    /// The login, as Claude Code 2.1.332 answered `get_status` (values made up), and an API
    /// key, whose value is never kept.
    #[test]
    fn the_status_says_who_claude_runs_as() {
        let status = |rows: Value| {
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "bowyer-status", "response": {"sections": [{"title": "Session", "rows": rows}, {"title": "Environment", "rows": [{"label": "Model", "value": "claude-opus-5-5"}]}]}}}).to_string()
        };
        let max = status(json!([
            {"label": "Version", "value": "2.1.332"},
            {"label": "Login method", "value": "Claude Max account"},
            {"label": "Organization", "value": "Example Org"},
            {"label": "Email", "value": "someone@example.com"},
        ]));
        assert_eq!(
            parse_account(&max),
            Some(Account { method: Some("Claude Max account".into()), email: Some("someone@example.com".into()), organization: Some("Example Org".into()) })
        );
        let keyed = status(json!([{"label": "Version", "value": "2.1.332"}, {"label": "API key", "value": "sk-ant-…abcd"}]));
        let account = parse_account(&keyed).unwrap();
        assert_eq!(account.method.as_deref(), Some("API key"));
        assert!(!format!("{account:?}").contains("sk-ant"), "the key's value is not kept");
        assert_eq!(parse_account(&status(json!([{"label": "Version", "value": "2.1.332"}]))), None, "nothing about the login");
        assert_eq!(parse_account("Not logged in"), None);
    }

    /// On a host the request runs in a folder of the user's own, never in `/tmp`.
    #[test]
    fn a_host_is_asked_in_a_folder_of_its_user() {
        let (program, args) = claude_command(false);
        assert_eq!(program, "sh");
        assert!(args[1].contains("umask 077") && args[1].contains("$HOME/.cache/bowyer/usage") && !args[1].contains("/tmp"), "{}", args[1]);
        assert!(args[3].starts_with("claude -p ") && args[3].ends_with("--setting-sources project"), "{}", args[3]);
    }

    /// The folder on a real host: made private, and the one the command runs in. Runs `pwd`
    /// in place of Claude Code. `BOWYER_SSH_HOST=user@host cargo test -p live -- --ignored
    /// folder_of_its_user_on`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_host_is_asked_in_a_folder_of_its_user_on_a_host() {
        use envs::Environment;
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let mut ssh = envs::SshEnvironment::new(&host);
        let system = std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe");
        if cfg!(windows) && system.is_file() {
            ssh.ssh = system.to_path_buf();
        }
        let (program, mut args) = claude_command(false);
        args[3] = "pwd; stat -c %a .".into();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = ssh.run(&program, &args, None).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].ends_with("/.cache/bowyer/usage"), "{output}");
        assert_eq!(lines[1], "700", "{output}");
    }

    /// Asks the Claude Code of this computer, signed in to a subscription. No model call:
    /// `cargo test -p live usage -- --ignored`.
    #[test]
    #[ignore = "needs Claude Code signed in to a subscription"]
    fn the_claude_code_here_tells_its_windows() {
        use envs::Environment;
        let (program, args) = claude_command(true);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let folder = std::env::temp_dir();
        let output = envs::LocalEnvironment::new().run_with_input(&program, &args, folder.to_str(), claude_request().as_bytes()).unwrap();
        let Plan::Limits { windows, .. } = parse_claude(&output) else { panic!("{output}") };
        assert!(windows.iter().any(|w| w.kind == WindowKind::FiveHours), "{windows:?}");
        assert!(windows.iter().any(|w| w.kind == WindowKind::Week && w.model.is_none()), "{windows:?}");
        assert!(output.contains("\"total_cost_usd\":0"), "no model call");
    }
}
