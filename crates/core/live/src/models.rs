//! The models a harness offers, as its own command lists them: `opencode models`
//! (`provider/model` a line), `agy models` (`id<TAB>label` a line, after a line of
//! progress) and `pi --list-models` (a table: provider, model, context…). The window asks
//! once per environment and harness, and searches the list as the user types.

use model::HarnessKind;

/// One model of a harness's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    /// What the turn asks for: `opencode-go/deepseek-v4-pro`, `gemini-4-neon-high`,
    /// `ollama/qwen3.8:27b`.
    pub id: String,
    /// What the list shows: the model's own name, or its id.
    pub label: String,
    /// The provider the model comes through, for the harnesses that have several.
    pub provider: Option<String>,
}

/// The program and arguments that list the models of `harness`, when it has such a
/// command. None of them starts a model.
pub fn list_command(harness: &HarnessKind) -> Option<(&'static str, &'static [&'static str])> {
    match harness {
        HarnessKind::OpenCode => Some(("opencode", &["models"])),
        HarnessKind::Antigravity => Some(("agy", &["models"])),
        HarnessKind::Pi => Some(("pi", &["--list-models"])),
        _ => None,
    }
}

/// The models in the output of [`list_command`].
pub fn parse_models(harness: &HarnessKind, output: &str) -> Vec<ModelEntry> {
    let lines = output.lines().map(|line| line.trim_end_matches('\r')).filter(|line| !line.trim().is_empty());
    match harness {
        HarnessKind::OpenCode => lines
            .filter_map(|line| {
                let id = line.trim();
                let (provider, model) = id.split_once('/')?;
                (!id.contains(char::is_whitespace) && !provider.is_empty() && !model.is_empty())
                    .then(|| ModelEntry { id: id.to_owned(), label: model.to_owned(), provider: Some(provider.to_owned()) })
            })
            .collect(),
        HarnessKind::Antigravity => lines
            .filter_map(|line| {
                let (id, label) = line.split_once('\t')?;
                let id = id.trim();
                (!id.is_empty() && !id.contains(' ')).then(|| ModelEntry { id: id.to_owned(), label: label.trim().to_owned(), provider: None })
            })
            .collect(),
        HarnessKind::Pi => lines
            .skip_while(|line| !line.trim_start().starts_with("provider"))
            .skip(1)
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let (provider, model) = (words.next()?, words.next()?);
                Some(ModelEntry { id: format!("{provider}/{model}"), label: model.to_owned(), provider: Some(provider.to_owned()) })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a model matches what the user typed: every word of `query` is in its id or
/// its label, case aside.
pub fn matches(entry: &ModelEntry, query: &str) -> bool {
    let haystack = format!("{} {}", entry.id, entry.label).to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
}

/// The providers of a list, in the order they first come.
pub fn providers(entries: &[ModelEntry]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for provider in entries.iter().filter_map(|entry| entry.provider.as_ref()) {
        if !found.contains(provider) {
            found.push(provider.clone());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_harness_lists_its_models_its_own_way() {
        let opencode = parse_models(&HarnessKind::OpenCode, "opencode-go/deepseek-v4-pro\r\nopenrouter/acme/coder-2\n\nsome warning line\n");
        assert_eq!(opencode.len(), 2);
        assert_eq!(opencode[1], ModelEntry { id: "openrouter/acme/coder-2".into(), label: "acme/coder-2".into(), provider: Some("openrouter".into()) });
        let agy = parse_models(&HarnessKind::Antigravity, "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\nclaude-sonnet-5-5-low\tClaude Sonnet 5.5 (Low)\n");
        assert_eq!(agy.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gemini-4-neon-high", "claude-sonnet-5-5-low"]);
        assert_eq!(agy[0].label, "Gemini 4 Neon (High)");
        let pi = parse_models(&HarnessKind::Pi, "provider  model           context  max-out\nollama    qwen3.8:27b     262.1K   16.4K\nanthropic claude-sonnet-5-5 1M 64K\n");
        assert_eq!(pi.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["ollama/qwen3.8:27b", "anthropic/claude-sonnet-5-5"]);
        assert_eq!(providers(&pi), ["ollama", "anthropic"]);
        assert!(parse_models(&HarnessKind::Claude, "x").is_empty());
    }

    #[test]
    fn a_search_needs_every_word() {
        let entry = ModelEntry { id: "openrouter/acme/coder-2".into(), label: "Acme Coder 2".into(), provider: Some("openrouter".into()) };
        assert!(matches(&entry, "acme CODER"));
        assert!(matches(&entry, ""));
        assert!(!matches(&entry, "acme gemini"));
    }
}
