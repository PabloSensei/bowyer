//! The MCP servers and plugins each harness is configured with, read from its own config
//! files through the environment, so remote hosts work too. Switching a Claude Code plugin
//! on or off writes `enabledPlugins` in its settings, after a backup of the file.

use std::io;

use envs::{Environment, ReadRange};
use model::HarnessKind;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionKind {
    Mcp,
    Plugin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    pub kind: ExtensionKind,
    pub name: String,
    /// Where it is configured: "user", "project", "local".
    pub scope: &'static str,
    pub enabled: bool,
    /// The command or URL of a server.
    pub detail: String,
    /// Whether the window can switch it.
    pub toggleable: bool,
}

fn read_json(env: &dyn Environment, path: &str) -> Option<Value> {
    let bytes = env.read(path, ReadRange::All).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn server_detail(server: &Value) -> String {
    if let Some(url) = server.get("url").and_then(Value::as_str) {
        return url.to_owned();
    }
    let command = server
        .get("command")
        .map(|command| match command {
            Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
            other => other.as_str().unwrap_or_default().to_owned(),
        })
        .unwrap_or_default();
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    format!("{command} {args}").trim().to_owned()
}

fn servers_of(map: Option<&Value>, scope: &'static str, disabled: &[String], out: &mut Vec<Extension>) {
    let Some(map) = map.and_then(Value::as_object) else { return };
    for (name, server) in map {
        let enabled = !disabled.contains(name) && server.get("enabled").and_then(Value::as_bool) != Some(false);
        out.push(Extension {
            kind: ExtensionKind::Mcp,
            name: name.clone(),
            scope,
            enabled,
            detail: server_detail(server),
            toggleable: false,
        });
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Server names of `[mcp_servers.NAME]` tables in the TOML of Codex, with `enabled`.
fn codex_servers(toml: &str) -> Vec<Extension> {
    let mut out: Vec<Extension> = Vec::new();
    for line in toml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[mcp_servers.") {
            let name = rest.trim_end_matches(']').trim_matches('"');
            if !name.contains('.') {
                out.push(Extension {
                    kind: ExtensionKind::Mcp,
                    name: name.to_owned(),
                    scope: "user",
                    enabled: true,
                    detail: String::new(),
                    toggleable: false,
                });
            }
            continue;
        }
        if line.starts_with('[') {
            continue;
        }
        let Some(current) = out.last_mut() else { continue };
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "enabled" => current.enabled = value.trim() != "false",
                "command" | "url" => current.detail = value.trim().trim_matches('"').to_owned(),
                _ => {}
            }
        }
    }
    out
}

pub fn list(env: &dyn Environment, harness: &HarnessKind, cwd: Option<&str>) -> io::Result<Vec<Extension>> {
    let home = env.home()?;
    let mut out = Vec::new();
    match harness {
        HarnessKind::Claude => {
            let global = read_json(env, &env.join(&home, ".claude.json"));
            let project = cwd.and_then(|cwd| {
                global.as_ref()?.get("projects")?.as_object()?.iter().find_map(|(path, value)| {
                    (path.replace('\\', "/").eq_ignore_ascii_case(&cwd.replace('\\', "/"))).then(|| value.clone())
                })
            });
            let disabled = strings(project.as_ref().and_then(|p| p.get("disabledMcpServers")));
            servers_of(global.as_ref().and_then(|g| g.get("mcpServers")), "user", &disabled, &mut out);
            servers_of(project.as_ref().and_then(|p| p.get("mcpServers")), "local", &disabled, &mut out);
            if let Some(cwd) = cwd {
                let disabled_json = strings(project.as_ref().and_then(|p| p.get("disabledMcpjsonServers")));
                let file = read_json(env, &env.join(cwd, ".mcp.json"));
                servers_of(file.as_ref().and_then(|f| f.get("mcpServers")), "project", &disabled_json, &mut out);
            }
            let settings = read_json(env, &env.join(&env.join(&home, ".claude"), "settings.json"));
            if let Some(plugins) = settings.as_ref().and_then(|s| s.get("enabledPlugins")).and_then(Value::as_object) {
                for (name, enabled) in plugins {
                    out.push(Extension {
                        kind: ExtensionKind::Plugin,
                        name: name.clone(),
                        scope: "user",
                        enabled: enabled.as_bool().unwrap_or(false),
                        detail: String::new(),
                        toggleable: env.id().is_local(),
                    });
                }
            }
        }
        HarnessKind::Codex => {
            let path = env.join(&env.join(&home, ".codex"), "config.toml");
            if let Ok(bytes) = env.read(&path, ReadRange::All) {
                out.extend(codex_servers(&String::from_utf8_lossy(&bytes)));
            }
        }
        HarnessKind::OpenCode => {
            let path = env.join(&env.join(&env.join(&home, ".config"), "opencode"), "opencode.json");
            let config = read_json(env, &path);
            servers_of(config.as_ref().and_then(|c| c.get("mcp")), "user", &[], &mut out);
            if let Some(plugins) = config.as_ref().and_then(|c| c.get("plugin")).and_then(Value::as_array) {
                for plugin in plugins.iter().filter_map(Value::as_str) {
                    out.push(Extension {
                        kind: ExtensionKind::Plugin,
                        name: plugin.to_owned(),
                        scope: "user",
                        enabled: true,
                        detail: String::new(),
                        toggleable: false,
                    });
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Switches a Claude Code plugin in `~/.claude/settings.json`, on this computer only. The
/// file as it was before the first switch is kept next to it as `settings.json.bowyer-backup`.
pub fn set_claude_plugin(home: &str, name: &str, enabled: bool) -> io::Result<()> {
    let path = std::path::Path::new(home).join(".claude").join("settings.json");
    let text = std::fs::read_to_string(&path)?;
    let mut settings: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
    let plugins = settings
        .as_object_mut()
        .ok_or_else(|| io::Error::other("settings are not an object"))?
        .entry("enabledPlugins")
        .or_insert_with(|| Value::Object(Default::default()));
    plugins
        .as_object_mut()
        .ok_or_else(|| io::Error::other("enabledPlugins is not an object"))?
        .insert(name.to_owned(), Value::Bool(enabled));
    // The first backup is the user's own file; later switches must not overwrite it.
    let backup = path.with_extension("json.bowyer-backup");
    if !backup.exists() {
        std::fs::write(&backup, &text)?;
    }
    let mut out = serde_json::to_string_pretty(&settings).map_err(io::Error::other)?;
    out.push('\n');
    std::fs::write(&path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_tables_become_servers() {
        let toml = "model = \"gpt-5\"\n[mcp_servers.docs]\ncommand = \"npx\"\n[mcp_servers.docs.env]\nKEY = \"x\"\n[mcp_servers.off]\nurl = \"http://x\"\nenabled = false\n";
        let servers = codex_servers(toml);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].detail, "npx");
        assert!(!servers[1].enabled);
    }

    #[test]
    fn plugin_switch_keeps_other_keys_and_a_backup() {
        let dir = std::env::temp_dir().join(format!("bowyer-ext-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/settings.json"), r#"{"model":"opus","enabledPlugins":{"a@m":true}}"#).unwrap();
        set_claude_plugin(&dir.to_string_lossy(), "a@m", false).unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(value["enabledPlugins"]["a@m"], Value::Bool(false));
        assert_eq!(value["model"], "opus");
        assert!(dir.join(".claude/settings.json.bowyer-backup").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
