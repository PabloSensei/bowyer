//! Versions of the harnesses in each environment, and their updates. The window asks each
//! harness for `--version` (on a host in one login shell), asks npm for the newest release
//! of its package, and shows an arrow where a newer one is out: on the harness's row in the
//! tree, by the harness's name in the composer, and in the new-session dialog. The update is
//! the harness's own command (`claude update`, `codex update`, `opencode upgrade`), run in
//! the window's terminal where the user sees it; when it ends the version is asked again.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use model::{EnvId, HarnessKind};
use ui_kit::prelude::*;
use ui_kit::*;

use super::new_session::HARNESSES;
use super::{Shell, now_ms};
use crate::i18n::t;
use crate::launch;

/// How long what was found stays fresh: an environment is asked again after that.
const RECHECK_MS: i64 = 12 * 3600 * 1000;
/// How soon a host that did not answer is asked again.
const RETRY_MS: i64 = 60 * 1000;
/// How often an update's terminal is looked at, to know when it ended.
const UPDATE_POLL: Duration = Duration::from_secs(2);
/// What starts a line of the host's answer, so nothing a login shell prints reads as one.
const MARK: &str = "bowyer-harness";

/// The program of a harness.
fn program(harness: &HarnessKind) -> String {
    launch::harness_command(harness, None).0
}

/// Whether the window asks the harness for its version. DeepSeek Harness has no known
/// `--version`, and an unknown word could start it.
fn asks_version(harness: &HarnessKind) -> bool {
    matches!(harness, HarnessKind::Claude | HarnessKind::Codex | HarnessKind::OpenCode | HarnessKind::Pi | HarnessKind::Antigravity)
}

/// The npm package whose newest release is the harness's. OpenCode 2 is a package of its
/// own: a version is compared with releases of its own line, never across.
pub(super) fn npm_package(harness: &HarnessKind, installed: &str) -> Option<&'static str> {
    match harness {
        HarnessKind::Claude => Some("@anthropic-ai/claude-code"),
        HarnessKind::Codex => Some("@openai/codex"),
        HarnessKind::OpenCode => Some(if update::harness::major(installed).is_some_and(|major| major >= 2) { "@opencode/cli" } else { "opencode-ai" }),
        _ => None,
    }
}

/// The harness's own update command; each finds out by itself how it was installed.
pub(super) fn update_args(harness: &HarnessKind) -> Option<&'static [&'static str]> {
    match harness {
        HarnessKind::Claude | HarnessKind::Codex | HarnessKind::Pi | HarnessKind::Antigravity => Some(&["update"]),
        HarnessKind::OpenCode => Some(&["upgrade"]),
        _ => None,
    }
}

/// The script that finds the harnesses of a host and their versions, in one login shell.
fn host_script() -> String {
    let mut body = String::new();
    for harness in HARNESSES.iter() {
        let name = program(harness);
        let version = if asks_version(harness) { format!("$({name} --version </dev/null 2>/dev/null | head -n 1)") } else { String::new() };
        body.push_str(&format!("command -v {name} >/dev/null 2>&1 && printf \"{MARK}\\t%s\\t%s\\n\" {name} \"{version}\"; "));
    }
    format!("exec \"${{SHELL:-sh}}\" -lc '{body}true'")
}

/// The harnesses in the host's answer to [`host_script`], with their versions.
fn parse_host(output: &str) -> Vec<(HarnessKind, Option<String>)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim_end_matches('\r').splitn(3, '\t');
            (parts.next()? == MARK).then_some(())?;
            let name = parts.next()?;
            let harness = HARNESSES.iter().find(|harness| program(harness) == name)?;
            Some((harness.clone(), parts.next().and_then(update::harness::version_in)))
        })
        .collect()
}

/// The harnesses installed in `env` and what each says its version is: on the PATH of this
/// computer, or of the login shell of the host. A host that did not answer is an error, not
/// a host without harnesses.
pub(super) fn harnesses_in(env: &dyn envs::Environment) -> std::io::Result<Vec<(HarnessKind, Option<String>)>> {
    if env.id().is_local() {
        return Ok(HARNESSES
            .iter()
            .filter_map(|harness| {
                let (path, prefix) = live::resolve_program(&program(harness))?;
                let version = asks_version(harness)
                    .then(|| {
                        let args: Vec<&str> = prefix.iter().map(String::as_str).chain(["--version"]).collect();
                        env.run(&path.to_string_lossy(), &args, None).ok()
                    })
                    .flatten()
                    .and_then(|output| update::harness::version_in(&output));
                Some((harness.clone(), version))
            })
            .collect());
    }
    Ok(parse_host(&env.run("sh", &["-c", &host_script()], None)?))
}

/// What the window knows of the harnesses' versions.
#[derive(Default)]
pub(super) struct HarnessVersions {
    /// What each environment has, and when it was asked: each harness with its version
    /// (`None` when it did not tell).
    found: HashMap<EnvId, (i64, HashMap<HarnessKind, Option<String>>)>,
    probing: HashSet<EnvId>,
    /// Environments that did not answer, and when: asked again after a short while.
    failed: HashMap<EnvId, i64>,
    /// The newest release of each npm package.
    latest: HashMap<&'static str, String>,
    latest_ms: i64,
    fetching: bool,
    /// Harnesses whose update runs now.
    updating: HashSet<(EnvId, HarnessKind)>,
    /// A release an update ended without reaching (the installer's channel lags npm, say):
    /// no arrow for it again.
    unreachable: HashMap<(EnvId, HarnessKind), String>,
}

impl HarnessVersions {
    /// The harnesses of an environment, once it was asked.
    pub(super) fn found(&self, env: &EnvId) -> Option<&HashMap<HarnessKind, Option<String>>> {
        self.found.get(env).map(|(_, found)| found)
    }

    /// Whether the environment did not answer the last time and nothing is known of it.
    pub(super) fn unknown(&self, env: &EnvId) -> bool {
        self.found(env).is_none() && self.failed.contains_key(env)
    }

    pub(super) fn version(&self, env: &EnvId, harness: &HarnessKind) -> Option<&str> {
        self.found(env)?.get(harness)?.as_deref()
    }

    /// The newer release of a harness, when one is out and not already tried.
    pub(super) fn newer(&self, env: &EnvId, harness: &HarnessKind) -> Option<String> {
        let installed = self.version(env, harness)?;
        let latest = self.latest.get(npm_package(harness, installed)?)?;
        let tried = self.unreachable.get(&(env.clone(), harness.clone())) == Some(latest);
        (update::harness::is_newer(latest, installed) && !tried).then(|| latest.clone())
    }

    pub(super) fn is_updating(&self, env: &EnvId, harness: &HarnessKind) -> bool {
        self.updating.contains(&(env.clone(), harness.clone()))
    }
}

/// What an ended update is checked against.
struct UpdateCheck {
    harness: HarnessKind,
    before: Option<String>,
    target: Option<String>,
}

impl Shell {
    /// Asks `env` for its harnesses' versions, unless that is known and fresh or asked now;
    /// `force` asks again. Called where versions show, so a host is asked once it is used.
    pub(super) fn want_versions(&mut self, env: &EnvId, force: bool, cx: &mut Context<Self>) {
        let fresh = self.harness_versions.found.get(env).is_some_and(|(at, _)| now_ms() - at < RECHECK_MS)
            || self.harness_versions.failed.get(env).is_some_and(|at| now_ms() - at < RETRY_MS);
        if self.harness_versions.probing.contains(env) || (fresh && !force) {
            return;
        }
        self.probe_versions(env.clone(), None, cx);
    }

    fn probe_versions(&mut self, env_id: EnvId, check: Option<UpdateCheck>, cx: &mut Context<Self>) {
        let Some(env) = self.env_for(&env_id) else { return };
        self.harness_versions.probing.insert(env_id.clone());
        let task = cx.background_spawn(async move { harnesses_in(&*env) });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            this.update(cx, |this, cx| {
                let versions = &mut this.harness_versions;
                versions.probing.remove(&env_id);
                let found = match found {
                    Ok(found) => found,
                    Err(error) => {
                        // What was known stays; the host is asked again in a while.
                        log::warn!("harness versions of {}: {error}", env_id.0);
                        versions.failed.insert(env_id.clone(), now_ms());
                        cx.notify();
                        return;
                    }
                };
                versions.failed.remove(&env_id);
                versions.found.insert(env_id.clone(), (now_ms(), found.into_iter().collect()));
                if let Some(check) = check {
                    let after = versions.version(&env_id, &check.harness).map(str::to_owned);
                    let key = (env_id.clone(), check.harness.clone());
                    if after.is_some() && after == check.before {
                        // The update did not move it: the arrow would ask again for nothing.
                        if let Some(target) = check.target {
                            versions.unreachable.insert(key, target);
                        }
                        this.status = Some(t("harness.not_updated").replace("{harness}", check.harness.label()));
                    } else if let Some(after) = after {
                        versions.unreachable.remove(&key);
                        this.status = Some(t("harness.updated").replace("{harness}", check.harness.label()).replace("{version}", &after));
                    }
                }
                this.want_latest(false, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks npm for the newest releases of the packages of the harnesses found, at most
    /// twice a day and only while automatic update checks are on; `force` asks now.
    fn want_latest(&mut self, force: bool, cx: &mut Context<Self>) {
        let auto = self.lib().setting("update.auto").is_none_or(|value| value != "false");
        let versions = &mut self.harness_versions;
        let fresh = now_ms() - versions.latest_ms < RECHECK_MS;
        if versions.fetching || (!force && (!auto || fresh)) {
            return;
        }
        let packages: HashSet<&'static str> = versions
            .found
            .values()
            .flat_map(|(_, found)| found.iter())
            .filter_map(|(harness, version)| npm_package(harness, version.as_deref()?))
            .collect();
        if packages.is_empty() {
            return;
        }
        versions.fetching = true;
        let task = cx.background_spawn(async move {
            packages.into_iter().filter_map(|package| Some((package, update::harness::npm_latest(package).ok()?))).collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let latest = task.await;
            this.update(cx, |this, cx| {
                let versions = &mut this.harness_versions;
                versions.fetching = false;
                versions.latest_ms = now_ms();
                versions.latest.extend(latest);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks the environment and npm again, now: "Check for updates".
    pub(super) fn check_versions(&mut self, env: &EnvId, cx: &mut Context<Self>) {
        self.want_versions(env, true, cx);
        self.want_latest(true, cx);
        self.status = Some(t("harness.checking").to_owned());
        cx.notify();
    }

    /// Whether a turn of the harness runs in the environment: its program is in use, and on
    /// Windows its file cannot be replaced.
    fn harness_busy(&self, env: &EnvId, harness: &HarnessKind) -> bool {
        self.runs.iter().any(|run| !run.finished && run.key.env == *env && run.key.harness == *harness)
    }

    /// Runs the harness's update in the window's terminal and, when that ends, asks its
    /// version again.
    pub(super) fn update_harness(&mut self, env_id: EnvId, harness: HarnessKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.harness_busy(&env_id, &harness) {
            self.status = Some(t("harness.busy").replace("{harness}", harness.label()));
            cx.notify();
            return;
        }
        let (Some(args), Some(env)) = (update_args(&harness), self.env_for(&env_id)) else { return };
        let line = launch::harness_tool_line(&*env, &program(&harness), args);
        let title = t("harness.update_title").replace("{harness}", harness.label());
        if !self.show_in_terminal(line, title, window, cx) {
            self.status = Some(t("harness.update_failed").replace("{harness}", harness.label()));
            cx.notify();
            return;
        }
        let Some(terminal) = self.terminals.last().map(|view| view.entity_id()) else { return };
        let check = UpdateCheck {
            harness: harness.clone(),
            before: self.harness_versions.version(&env_id, &harness).map(str::to_owned),
            target: self.harness_versions.newer(&env_id, &harness),
        };
        self.harness_versions.updating.insert((env_id.clone(), harness.clone()));
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(UPDATE_POLL).await;
                let running = this
                    .update(cx, |this, cx| this.terminals.iter().any(|view| view.entity_id() == terminal && !view.read(cx).is_closed()))
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.harness_versions.updating.remove(&(env_id.clone(), harness));
                this.probe_versions(env_id, Some(check), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The version of a harness in an environment and, when a newer one is out, an arrow
    /// that updates it. `with_version` shows the number too; `with_target` the newer one.
    pub(super) fn version_mark(&mut self, env: &EnvId, harness: &HarnessKind, id: SharedString, with_version: bool, with_target: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.want_versions(env, false, cx);
        let theme = *cx.global::<Theme>();
        let versions = &self.harness_versions;
        let installed = versions.version(env, harness).map(str::to_owned);
        let updating = versions.is_updating(env, harness);
        let newer = versions.newer(env, harness).filter(|_| update_args(harness).is_some());
        if installed.is_none() || (!with_version && newer.is_none() && !updating) {
            return None;
        }
        let place = if env.is_local() { t("group.local").to_owned() } else { env.ssh_host().unwrap_or(&env.0).to_owned() };
        let arrow = (!updating).then_some(newer).flatten().map(|newer| {
            let tip = t("harness.update_tip")
                .replace("{harness}", harness.label())
                .replace("{from}", installed.as_deref().unwrap_or(""))
                .replace("{to}", &newer)
                .replace("{env}", &place);
            let (env, harness) = (env.clone(), harness.clone());
            div()
                .id(id.clone())
                .flex()
                .flex_none()
                .items_center()
                .gap(px(3.))
                .px(px(4.))
                .h(px(18.))
                .rounded(px(4.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.accent)
                .hover(|style| style.bg(theme.hover))
                .child("↑")
                .when(with_target, |this| this.child(newer.clone()))
                .tooltip(move |_, cx| tooltip(tip.clone(), None, cx))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.update_harness(env.clone(), harness.clone(), window, cx);
                }))
        });
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .when(with_version, |this| {
                    this.children(installed.map(|version| div().text_size(px(11.)).font_weight(FontWeight::NORMAL).text_color(theme.text_muted).child(version)))
                })
                .when(updating, |this| this.child(div().text_size(px(11.)).text_color(theme.accent).child(t("harness.updating"))))
                .children(arrow)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the prelude of the UI brings an attribute `test` of its own.
    use super::{HarnessVersions, harnesses_in, host_script, npm_package, parse_host, update_args};
    use model::{EnvId, HarnessKind};

    /// A host that does not answer is not taken for one without harnesses.
    #[test]
    fn an_unreachable_host_is_an_error() {
        let mut ssh = envs::SshEnvironment::new("nobody@nowhere");
        ssh.ssh = std::path::PathBuf::from("Z:/no/such/ssh");
        assert!(harnesses_in(&ssh).is_err());
    }

    #[test]
    fn the_host_answer_is_read_by_its_marks_only() {
        let answer = "Welcome to the host\nbowyer-harness\tclaude\t2.1.300 (Claude Code)\nbowyer-harness\topencode\topencode v2.3.0\r\nbowyer-harness\tdsh\t\nclaude 9.9.9\n";
        let found = parse_host(answer);
        assert_eq!(
            found,
            vec![
                (HarnessKind::Claude, Some("2.1.300".to_owned())),
                (HarnessKind::OpenCode, Some("2.3.0".to_owned())),
                (HarnessKind::DeepSeek, None),
            ]
        );
    }

    #[test]
    fn the_host_script_asks_only_harnesses_that_tell_a_version() {
        let script = host_script();
        assert!(script.contains("claude --version") && script.contains("codex --version") && script.contains("opencode --version"));
        assert!(!script.contains("dsh --version"), "{script}");
        // It runs inside single quotes: none of its own.
        let inner = script.split_once("-lc '").map(|(_, rest)| rest.trim_end_matches('\'')).unwrap_or_default();
        assert!(!inner.contains('\''), "{inner}");
    }

    /// The script in a real POSIX shell, with stand-ins for two harnesses first on PATH.
    /// Needs `sh` (Git Bash on Windows): run with `--ignored`.
    #[test]
    #[ignore]
    fn the_host_script_runs_in_a_posix_shell() {
        let dir = std::env::temp_dir().join(format!("bowyer-harness-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude"), "#!/bin/sh\necho '2.1.300 (Claude Code)'\n").unwrap();
        std::fs::write(dir.join("opencode"), "#!/bin/sh\necho 'opencode v2.3.0'\n").unwrap();
        let unix_dir = std::process::Command::new("sh").args(["-c", "cd \"$1\" && pwd", "sh", &dir.to_string_lossy()]).output().unwrap();
        let unix_dir = String::from_utf8_lossy(&unix_dir.stdout).trim().to_owned();
        let path = format!("{unix_dir}:/usr/bin:/bin");
        let script = format!("chmod +x \"{unix_dir}\"/*; PATH=\"{path}\"; export PATH; SHELL=sh; export SHELL; {}", host_script());
        let output = std::process::Command::new("sh").args(["-c", &script]).env("PATH", std::env::var("PATH").unwrap_or_default()).output().unwrap();
        let found = parse_host(&String::from_utf8_lossy(&output.stdout));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found.contains(&(HarnessKind::Claude, Some("2.1.300".to_owned()))), "{found:?} {}", String::from_utf8_lossy(&output.stderr));
        assert!(found.contains(&(HarnessKind::OpenCode, Some("2.3.0".to_owned()))), "{found:?}");
    }

    #[test]
    fn opencode_is_compared_within_its_own_line() {
        assert_eq!(npm_package(&HarnessKind::OpenCode, "2.3.0"), Some("@opencode/cli"));
        assert_eq!(npm_package(&HarnessKind::OpenCode, "1.19.2"), Some("opencode-ai"));
        assert_eq!(npm_package(&HarnessKind::Claude, "2.1.300"), Some("@anthropic-ai/claude-code"));
        assert_eq!(npm_package(&HarnessKind::DeepSeek, "1.0.0"), None);
        assert_eq!(update_args(&HarnessKind::OpenCode), Some(&["upgrade"][..]));
        assert_eq!(update_args(&HarnessKind::DeepSeek), None);
    }

    #[test]
    fn a_newer_release_shows_until_an_update_could_not_reach_it() {
        let env = EnvId::local();
        let mut versions = HarnessVersions::default();
        versions.found.insert(env.clone(), (0, [(HarnessKind::Claude, Some("2.1.300".to_owned())), (HarnessKind::DeepSeek, None)].into_iter().collect()));
        assert_eq!(versions.newer(&env, &HarnessKind::Claude), None, "nothing known of npm yet");
        versions.latest.insert("@anthropic-ai/claude-code", "2.1.331".to_owned());
        assert_eq!(versions.newer(&env, &HarnessKind::Claude).as_deref(), Some("2.1.331"));
        assert_eq!(versions.newer(&env, &HarnessKind::DeepSeek), None);
        versions.unreachable.insert((env.clone(), HarnessKind::Claude), "2.1.331".to_owned());
        assert_eq!(versions.newer(&env, &HarnessKind::Claude), None, "an update that did not reach it asks no more");
        versions.latest.insert("@anthropic-ai/claude-code", "2.1.332".to_owned());
        assert_eq!(versions.newer(&env, &HarnessKind::Claude).as_deref(), Some("2.1.332"), "a later release asks again");
    }
}
