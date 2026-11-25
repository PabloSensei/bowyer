//! The commands and skills Claude Code takes after `/`: the user's and the project's own
//! (`.claude/commands/**/*.md`, `.claude/skills/*/SKILL.md`) and those of the plugins
//! turned on, with the description and argument hint of their front matter. Read through
//! the environment, so a host's are listed as well; no harness is started for it.

use envs::{Environment, ReadRange};
use serde_json::Value;

/// Where a command comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandSource {
    UserCommand,
    ProjectCommand,
    UserSkill,
    ProjectSkill,
    /// A plugin's command or skill, with the plugin's name.
    Plugin(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessCommand {
    /// What follows the `/`: `review`, `frontend:lint` for a command in a subfolder,
    /// `plugin:command` for a plugin's.
    pub name: String,
    pub description: String,
    /// What the command takes after its name, as its author wrote it (`<file> [focus]`).
    pub hint: Option<String>,
    pub source: CommandSource,
}

/// The fields of a Markdown file's front matter, and its first line of text after it.
fn front_matter(text: &str) -> (Vec<(String, String)>, Option<String>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut lines = text.lines().peekable();
    if lines.peek().map(|line| line.trim_end()) == Some("---") {
        lines.next();
        let mut folded: Option<usize> = None;
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                break;
            }
            // A value on the lines below its key (`description: >`), indented.
            if let Some(index) = folded {
                if line.starts_with([' ', '\t']) {
                    let value = &mut fields[index].1;
                    if !value.is_empty() {
                        value.push(' ');
                    }
                    value.push_str(line.trim());
                    continue;
                }
                folded = None;
            }
            let Some((key, value)) = line.split_once(':') else { continue };
            if key.starts_with([' ', '\t']) {
                continue;
            }
            let value = value.trim();
            let value = if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
                &value[1..value.len() - 1]
            } else {
                value
            };
            if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
                folded = Some(fields.len());
                fields.push((key.trim().to_owned(), String::new()));
            } else {
                fields.push((key.trim().to_owned(), value.to_owned()));
            }
        }
    }
    let first = lines.map(str::trim).find(|line| !line.is_empty()).map(|line| line.trim_start_matches('#').trim().to_owned());
    (fields, first)
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()).filter(|value| !value.is_empty())
}

/// A command or skill from its file: the description of its front matter, else its first
/// line. A skill only the model may use (`user-invocable: false`) is left out.
fn command_of(name: String, text: &str, source: CommandSource) -> Option<HarnessCommand> {
    let (fields, first) = front_matter(text);
    if field(&fields, "user-invocable") == Some("false") {
        return None;
    }
    let description = field(&fields, "description").map(str::to_owned).or(first).unwrap_or_default();
    let hint = field(&fields, "argument-hint").map(str::to_owned);
    Some(HarnessCommand { name, description, hint, source })
}

/// A place commands and skills come from: a folder with `commands/` and `skills/`, the
/// prefix their names take, and the sources they are.
struct Root {
    dir: String,
    prefix: String,
    command: CommandSource,
    skill: CommandSource,
}

fn read_json(env: &dyn Environment, path: &str) -> Option<Value> {
    serde_json::from_slice(&env.read(path, ReadRange::All).ok()?).ok()
}

/// The plugins turned on, in the user's settings or the project's, with their folders.
fn plugin_roots(env: &dyn Environment, claude: &str, cwd: &str) -> Vec<Root> {
    let mut enabled: Vec<String> = Vec::new();
    for settings in [env.join(claude, "settings.json"), env.join(&env.join(cwd, ".claude"), "settings.json"), env.join(&env.join(cwd, ".claude"), "settings.local.json")] {
        let Some(plugins) = read_json(env, &settings).and_then(|value| value.get("enabledPlugins").cloned()) else { continue };
        for (name, on) in plugins.as_object().into_iter().flatten() {
            enabled.retain(|known| known != name);
            if on.as_bool() == Some(true) {
                enabled.push(name.clone());
            }
        }
    }
    let Some(installed) = read_json(env, &env.join(&env.join(claude, "plugins"), "installed_plugins.json")) else { return Vec::new() };
    let plugins = installed.get("plugins").unwrap_or(&installed);
    let mut roots = Vec::new();
    for id in enabled {
        let Some(entry) = plugins.get(&id) else { continue };
        // One install per scope; the first is the one in use.
        let entry = entry.as_array().and_then(|all| all.first()).unwrap_or(entry);
        let Some(dir) = entry.get("installPath").and_then(Value::as_str) else { continue };
        let name = id.split('@').next().unwrap_or(&id).to_owned();
        roots.push(Root { dir: dir.to_owned(), prefix: format!("{name}:"), command: CommandSource::Plugin(name.clone()), skill: CommandSource::Plugin(name) });
    }
    roots
}

/// The commands and skills Claude Code takes in `cwd` on the machine of `env`, sorted by
/// name; a project's own come before the user's of the same name.
pub fn claude_commands(env: &dyn Environment, cwd: &str) -> Vec<HarnessCommand> {
    let Ok(home) = env.home() else { return Vec::new() };
    let claude = env.join(&home, ".claude");
    let mut roots = vec![
        Root { dir: env.join(cwd, ".claude"), prefix: String::new(), command: CommandSource::ProjectCommand, skill: CommandSource::ProjectSkill },
        Root { dir: claude.clone(), prefix: String::new(), command: CommandSource::UserCommand, skill: CommandSource::UserSkill },
    ];
    roots.extend(plugin_roots(env, &claude, cwd));
    // The files first, then their heads in one read: one round trip on a host.
    let mut found: Vec<(String, String, CommandSource)> = Vec::new();
    for root in &roots {
        let commands = env.join(&root.dir, "commands");
        for meta in env.find_files(&commands, ".md", 4).unwrap_or_default() {
            let relative = meta.path.strip_prefix(commands.as_str()).unwrap_or(&meta.path);
            let name = relative.trim_start_matches(['/', '\\']).trim_end_matches(".md").replace(['/', '\\'], ":");
            if !name.is_empty() {
                found.push((format!("{}{name}", root.prefix), meta.path, root.command.clone()));
            }
        }
        let skills = env.join(&root.dir, "skills");
        for meta in env.find_files(&skills, "SKILL.md", 2).unwrap_or_default() {
            let folder = meta.path.trim_end_matches("SKILL.md").trim_end_matches(['/', '\\']);
            let name = folder.rsplit(['/', '\\']).next().unwrap_or("");
            if !name.is_empty() {
                found.push((format!("{}{name}", root.prefix), meta.path, root.skill.clone()));
            }
        }
    }
    let requests: Vec<(String, ReadRange)> = found.iter().map(|(_, path, _)| (path.clone(), ReadRange::Head(4096))).collect();
    let heads = env.read_many(&requests);
    let mut out: Vec<HarnessCommand> = Vec::new();
    for ((name, _, source), head) in found.into_iter().zip(heads) {
        if out.iter().any(|known| known.name == name) {
            continue;
        }
        let text = head.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
        out.extend(command_of(name, &text, source));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    #[test]
    fn front_matter_gives_the_description_and_hint() {
        let (fields, first) = front_matter("---\ndescription: \"Review a file\"\nargument-hint: <file> [focus]\n---\n\n# Review\nRead $1.\n");
        assert_eq!(field(&fields, "description"), Some("Review a file"));
        assert_eq!(field(&fields, "argument-hint"), Some("<file> [focus]"));
        assert_eq!(first.as_deref(), Some("Review"));
        let (fields, _) = front_matter("---\nname: x\ndescription: >\n  Two lines\n  folded.\nallowed-tools: Bash\n---\nbody");
        assert_eq!(field(&fields, "description"), Some("Two lines folded."));
        assert_eq!(field(&fields, "allowed-tools"), Some("Bash"));
        let (fields, first) = front_matter("Just a prompt for $ARGUMENTS.\n");
        assert!(fields.is_empty());
        assert_eq!(first.as_deref(), Some("Just a prompt for $ARGUMENTS."));
    }

    /// The user's and the project's commands and skills, a command in a subfolder, and a
    /// plugin's that is on; a plugin that is off and a skill only the model uses are not
    /// listed, and the project's command wins over the user's of the same name.
    #[test]
    fn claude_commands_come_from_the_user_the_project_and_plugins() {
        let home = TempDir::new("commands-home");
        let env = LocalEnvironment::with_home(home.path());
        let write = |path: &str, text: &str| {
            let path = home.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(".claude/commands/review.md", "---\ndescription: User review\n---\n");
        write(".claude/commands/frontend/lint.md", "Lint the frontend.");
        write(".claude/skills/notes/SKILL.md", "---\nname: notes\ndescription: Keep notes\n---\n");
        write(".claude/skills/hidden/SKILL.md", "---\ndescription: x\nuser-invocable: false\n---\n");
        write("proj/.claude/commands/review.md", "---\ndescription: Project review\nargument-hint: <pr>\n---\n");
        write("proj/.claude/skills/deploy/SKILL.md", "---\ndescription: Deploy it\n---\n");
        let plugin = home.path().join("plugins-store/tools");
        let plugin_off = home.path().join("plugins-store/off");
        write("plugins-store/tools/commands/fmt.md", "---\ndescription: Format\n---\n");
        write("plugins-store/tools/skills/trace/SKILL.md", "---\ndescription: Trace it\n---\n");
        write("plugins-store/off/commands/nope.md", "nope");
        let installed = serde_json::json!({"version": 2, "plugins": {
            "tools@market": [{"scope": "user", "installPath": plugin.to_string_lossy()}],
            "off@market": [{"scope": "user", "installPath": plugin_off.to_string_lossy()}],
        }});
        write(".claude/plugins/installed_plugins.json", &installed.to_string());
        write(".claude/settings.json", r#"{"enabledPlugins":{"tools@market":true,"off@market":false}}"#);
        let cwd = home.path().join("proj").to_string_lossy().into_owned();
        let listed: Vec<(String, String, Option<String>, CommandSource)> =
            claude_commands(&env, &cwd).into_iter().map(|c| (c.name, c.description, c.hint, c.source)).collect();
        let plugin_source = CommandSource::Plugin("tools".into());
        assert_eq!(
            listed,
            vec![
                ("deploy".into(), "Deploy it".into(), None, CommandSource::ProjectSkill),
                ("frontend:lint".into(), "Lint the frontend.".into(), None, CommandSource::UserCommand),
                ("notes".into(), "Keep notes".into(), None, CommandSource::UserSkill),
                ("review".into(), "Project review".into(), Some("<pr>".into()), CommandSource::ProjectCommand),
                ("tools:fmt".into(), "Format".into(), None, plugin_source.clone()),
                ("tools:trace".into(), "Trace it".into(), None, plugin_source),
            ]
        );
    }
}
