//! Runs one real turn and prints what arrives, with timings: `cargo run -p live --example
//! turn -- <claude|opencode> <model> <cwd> [session] -- prompt`. It spends real tokens.
use std::time::Instant;

use live::{LiveEvent, Permission, TurnSpec, start};
use model::HarnessKind;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").expect("-- before the prompt");
    let (head, prompt) = (&args[..split], args[split + 1..].join(" "));
    let spec = TurnSpec {
        harness: HarnessKind::from_id(&head[0]),
        model: Some(head[1].clone()),
        effort: None,
        cwd: head[2].clone(),
        session: head.get(3).cloned(),
        prompt,
        // BOWYER_PERMISSION=settings|accept_edits|bypass; questions are answered with the
        // option kind in BOWYER_ANSWER (allow_once by default).
        permission: Permission::ALL
            .into_iter()
            .find(|p| std::env::var("BOWYER_PERMISSION").is_ok_and(|v| v == p.id()))
            .unwrap_or(Permission::AcceptEdits),
        // BOWYER_SSH=user@host and BOWYER_SSH_KEY=path run the turn on that host.
        // BOWYER_ATTACH=path,path sends files; pictures go inline.
        attachments: std::env::var("BOWYER_ATTACH")
            .map(|list| {
                list.split(',')
                    .map(|path| live::Attachment {
                        path: path.to_owned(),
                        image: live::image_type(path).map(|media| (media.to_owned(), std::sync::Arc::new(std::fs::read(path).expect("attachment")))),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ssh: std::env::var("BOWYER_SSH").ok().map(|host| {
            envs::SshEnvironment::new(&host).with_identity(std::env::var_os("BOWYER_SSH_KEY").map(Into::into))
        }),
        ide: None,
        // BOWYER_DROP="<kept line> <failed prompt>" resumes before a failed turn.
        drop_turn: std::env::var("BOWYER_DROP").ok().and_then(|value| {
            let (kept, prompt) = value.split_once(' ')?;
            Some((kept.to_owned(), prompt.to_owned()))
        }),
        mcp_config: None,
    };
    let started = Instant::now();
    let mut turn = start(spec).expect("start");
    let mut first_update = None;
    let mut updates = 0;
    let mut last = None;
    loop {
        match futures_executor_block(&mut turn) {
            Some(LiveEvent::Session(id)) => println!("{:>6}ms session {id}", started.elapsed().as_millis()),
            Some(LiveEvent::Update(t)) => {
                updates += 1;
                first_update.get_or_insert(started.elapsed().as_millis());
                last = Some(t);
            }
            Some(LiveEvent::Stderr(_)) => {}
            Some(LiveEvent::Permission(ask)) => {
                let wanted = std::env::var("BOWYER_ANSWER").unwrap_or_else(|_| "allow_once".into());
                let choice = ask.options.iter().find(|(_, kind, _)| *kind == wanted).map(|(id, _, _)| id.clone());
                println!("{:>6}ms asked: {} [{}] -> {choice:?}", started.elapsed().as_millis(), ask.title, ask.kind);
                let _ = ask.reply.send(choice);
            }
            Some(LiveEvent::Finished { ok, error }) => {
                println!("{:>6}ms finished ok={ok} error={error:?}", started.elapsed().as_millis());
                break;
            }
            None => break,
        }
    }
    println!("first update at {first_update:?}ms, {updates} updates");
    if let Some(t) = last {
        for entry in &t.entries {
            println!("  {:?}: {} blocks", entry.role, entry.blocks.len());
        }
        println!("  context {:?}, cost {:?}, tasks {}", t.last_usage.map(|u| u.context()), t.cost_usd, t.tasks(false).len());
        if let Some(Some(model::Block::Text { text })) = t.entries.last().map(|e| e.blocks.iter().rev().find(|b| matches!(b, model::Block::Text { .. }))) {
            println!("  answer: {}", model::shorten(text.trim(), 300));
        }
    }
}

fn futures_executor_block(turn: &mut live::Turn) -> Option<LiveEvent> {
    loop {
        match turn.events.try_recv() {
            Ok(event) => return Some(event),
            Err(error) if error.is_closed() => return None,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}
