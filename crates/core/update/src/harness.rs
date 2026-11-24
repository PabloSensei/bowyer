//! Versions of the harnesses Bowyer drives: what `--version` printed, the newest release of
//! their npm package, and whether one is newer than the other. The harnesses update
//! themselves; this only tells when there is something to update to.

use crate::{Error, Result, transfer};

/// The version in what a harness printed for `--version`: the first word that reads as one.
/// "2.1.329 (Claude Code)", "opencode v2.3.0" and "codex-cli 0.172.0" all give theirs.
pub fn version_in(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '+');
        let word = word.strip_prefix(['v', 'V']).unwrap_or(word);
        let numeric = word.split(['-', '+']).next().unwrap_or_default();
        let parts: Vec<&str> = numeric.split('.').collect();
        (parts.len() >= 2 && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))).then(|| word.to_owned())
    })
}

/// The address of the newest release of an npm package (`@scope/name` keeps its slash
/// escaped, as the registry wants it).
pub fn npm_latest_url(package: &str) -> String {
    format!("https://registry.npmjs.org/{}/latest", package.replace('/', "%2f"))
}

/// The version in the registry's answer about a release.
pub fn version_of_release(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("version")?.as_str().map(str::to_owned)
}

/// The newest release of an npm package, asked from the registry.
pub fn npm_latest(package: &str) -> Result<String> {
    let body = transfer(&npm_latest_url(package), None)?;
    version_of_release(&body).ok_or_else(|| Error::Feed(format!("no version for {package}")))
}

/// The major number of a version: "2" of "2.3.0".
pub fn major(version: &str) -> Option<u64> {
    version.trim_start_matches(['v', 'V']).split('.').next()?.parse().ok()
}

/// Whether `latest` is a newer version than `installed`. Versions that do not read as such
/// are never newer, so a strange answer shows no update.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    let parse = |version: &str| {
        let version = version.trim_start_matches(['v', 'V']);
        semver::Version::parse(version).ok().or_else(|| {
            // "1.2" and the like: fill the missing parts.
            let (numeric, rest) = version.split_once(['-', '+']).map_or((version, ""), |(a, _)| (a, &version[a.len()..]));
            let mut parts: Vec<&str> = numeric.split('.').collect();
            while parts.len() < 3 {
                parts.push("0");
            }
            semver::Version::parse(&format!("{}{rest}", parts.join("."))).ok()
        })
    };
    match (parse(latest), parse(installed)) {
        (Some(latest), Some(installed)) => latest > installed,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_read_from_what_each_harness_prints() {
        assert_eq!(version_in("2.1.329 (Claude Code)").as_deref(), Some("2.1.329"));
        assert_eq!(version_in("opencode v2.3.0").as_deref(), Some("2.3.0"));
        assert_eq!(version_in("codex-cli 0.172.0").as_deref(), Some("0.172.0"));
        assert_eq!(version_in("tool 1.4.0-beta.2\n").as_deref(), Some("1.4.0-beta.2"));
        assert_eq!(version_in("version: (1.2)").as_deref(), Some("1.2"));
        assert_eq!(version_in("command not found"), None);
        assert_eq!(version_in("run 3 times"), None, "a lone number is no version");
        assert_eq!(version_in(""), None);
    }

    #[test]
    fn newer_compares_versions_not_text() {
        assert!(is_newer("2.1.331", "2.1.329"));
        assert!(is_newer("2.1.10", "2.1.9"));
        assert!(!is_newer("2.1.329", "2.1.329"));
        assert!(!is_newer("0.171.0", "0.172.0"));
        assert!(is_newer("1.0.0", "1.0.0-beta.1"));
        assert!(is_newer("v2.4.1", "2.3"));
        assert!(!is_newer("latest", "2.3.0"), "what is no version is never newer");
        assert_eq!(major("2.3.0"), Some(2));
        assert_eq!(major("v1.19.6"), Some(1));
    }

    #[test]
    fn the_registry_is_asked_for_the_package_with_its_scope() {
        assert_eq!(npm_latest_url("@opencode/cli"), "https://registry.npmjs.org/@opencode%2fcli/latest");
        assert_eq!(npm_latest_url("opencode-ai"), "https://registry.npmjs.org/opencode-ai/latest");
        assert_eq!(version_of_release(br#"{"name":"x","version":"0.172.0"}"#).as_deref(), Some("0.172.0"));
        assert_eq!(version_of_release(b"not json"), None);
    }
}
