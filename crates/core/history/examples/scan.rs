//! Scans the real local history read-only and prints counts and timings, no content.
use std::time::Instant;

use envs::{Environment, LocalEnvironment, SshEnvironment};
use history::builtin_sources;

fn main() {
    // BOWYER_SSH=user@host (and BOWYER_SSH_KEY=path) scans that host instead.
    let env: Box<dyn Environment> = match std::env::var("BOWYER_SSH") {
        Ok(host) => Box::new(SshEnvironment::new(&host).with_identity(std::env::var_os("BOWYER_SSH_KEY").map(Into::into))),
        Err(_) => Box::new(LocalEnvironment::new()),
    };
    let env = &*env;
    let cache = std::env::temp_dir().join("harness-scan-cache");
    for source in builtin_sources() {
        let started = Instant::now();
        match source.scan(env, &(), &cache) {
            Ok(sessions) => {
                let titled = sessions.iter().filter(|s| !s.title.is_empty()).count();
                let with_cwd = sessions.iter().filter(|s| s.cwd.is_some()).count();
                println!("{}: {} sessions ({titled} titled, {with_cwd} with cwd) in {:?}", source.harness(), sessions.len(), started.elapsed());
                let mut sorted = sessions.clone();
                sorted.sort_by_key(|s| std::cmp::Reverse(s.source.size));
                for summary in sorted.iter().take(1).chain(sessions.iter().max_by_key(|s| s.updated_ms)) {
                    let started = Instant::now();
                    match source.load(env, summary, &cache) {
                        Ok(t) => println!("  load {} KB: {} entries, {} changes, {} tasks, ctx {:?}, total out {} in {:?}",
                            summary.source.size / 1024, t.entries.len(), t.changes().len(), t.tasks(false).len(),
                            t.last_usage.map(|u| u.context()), t.total_usage.output, started.elapsed()),
                        Err(e) => println!("  load error: {e}"),
                    }
                }
            }
            Err(error) => println!("{}: error {error}", source.harness()),
        }
    }
}
