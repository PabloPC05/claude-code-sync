//! `claude-code-sync hook <event>`: what Claude Code's hooks run.
//!
//! A hook must never break or block the person's Claude, so every path out
//! of here exits 0 and says nothing unless it means to block a prompt. They
//! are also on the way of every prompt and every chat start: no network, no
//! git, and the prompt hook returns at once when nothing is stale.

use std::io::Read;

/// The hook events this binary handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `UserPromptSubmit`: block a prompt typed in an out-of-date window.
    Prompt,
    /// `SessionStart`: a window loaded its chat; it is up to date.
    SessionStart,
    /// `SessionEnd`: repair what a stale window appended, then sync.
    SessionEnd,
    /// `Stop`: a turn finished; sync so the other PC gets it soon.
    Stop,
    /// Print this process's parent and grandparent ids (for tests).
    Whoami,
}

/// What a blocked prompt shows the person. `/resume` with the chat's id is
/// the way: the picker never lists the chat open in the window it runs in.
pub fn block_reason(session_id: &str) -> String {
    format!(
        "Este chat ha seguido en tu otro PC y esta ventana tiene la versión de antes, así que no he enviado tu mensaje. Para cargar lo último escribe /resume {session_id} y luego vuelve a enviarlo."
    )
}

/// Run a hook. Never fails and never panics outward.
pub fn run(event: Event) {
    let outcome = std::panic::catch_unwind(|| {
        if let Err(e) = handle(event) {
            log::debug!("hook {event:?}: {e:#}");
        }
    });
    drop(outcome);
}

fn handle(event: Event) -> anyhow::Result<()> {
    match event {
        Event::Prompt => {
            if !crate::stale::any_marker() {
                return Ok(());
            }
            let mut input = String::new();
            std::io::stdin()
                .take(1 << 20)
                .read_to_string(&mut input)
                .ok();
            if let Some(reason) = prompt_verdict(&input, &crate::stale::claude_ancestors()) {
                println!("{}", block_json(&reason));
            }
        }
        Event::SessionStart => {
            if crate::stale::any_marker() {
                for pid in crate::stale::claude_ancestors() {
                    if let Err(e) = crate::stale::refresh(pid) {
                        log::debug!("refresh {pid}: {e:#}");
                    }
                }
            }
            // After the refresh, so the run marks this window again only if
            // the chat moved on since it loaded.
            crate::autosync::spawn_detached()?;
        }
        Event::SessionEnd => {
            if crate::stale::any_marker() {
                for pid in crate::stale::claude_ancestors() {
                    if let Err(e) = crate::stale::ended(pid) {
                        log::debug!("ended {pid}: {e:#}");
                    }
                }
            }
            crate::autosync::spawn_detached()?;
        }
        Event::Stop => crate::autosync::spawn_detached()?,
        Event::Whoami => {
            for pid in crate::stale::claude_ancestors() {
                println!("{pid}");
            }
        }
    }
    Ok(())
}

/// `Some(reason)` when the prompt typed in the window above `ancestors`
/// must be blocked: the hook's input names a chat that window is behind on.
pub fn prompt_verdict(input: &str, ancestors: &[u32]) -> Option<String> {
    let session_id = serde_json::from_str::<serde_json::Value>(input)
        .ok()?
        .get("session_id")?
        .as_str()?
        .to_string();
    crate::stale::stale_for(&session_id, ancestors).map(|_| block_reason(&session_id))
}

/// The JSON a `UserPromptSubmit` hook prints to stop the prompt.
pub fn block_json(reason: &str) -> String {
    serde_json::json!({ "decision": "block", "reason": reason }).to_string()
}
