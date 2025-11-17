use crate::sessions::{
    SessionSummary, find_codex_sessions_for_current_dir, find_codex_sessions_for_dir,
};
use crate::{CliResult, SessionCommand};

pub async fn handle_session_cmd(cmd: SessionCommand) -> CliResult<()> {
    match cmd {
        SessionCommand::List { limit, path } => {
            let sessions: Vec<SessionSummary> = if let Some(p) = path {
                let root = std::path::PathBuf::from(p);
                find_codex_sessions_for_dir(&root, limit).await?
            } else {
                find_codex_sessions_for_current_dir(limit).await?
            };
            if sessions.is_empty() {
                println!("No Codex sessions found under ~/.codex/sessions");
            } else {
                println!("Recent Codex sessions (newest first):");
                for s in sessions {
                    let updated = s.updated_at.as_deref().unwrap_or("-");
                    let cwd = s.cwd.as_deref().unwrap_or("-");
                    let preview_raw = s
                        .first_user_message
                        .as_deref()
                        .unwrap_or("")
                        .replace('\n', " ");
                    let preview = super::doctor::truncate_for_display(&preview_raw, 80);

                    println!("- id: {}", s.id);
                    println!("  updated: {} | cwd: {}", updated, cwd);
                    if !preview.is_empty() {
                        println!("  prompt: {}", preview);
                    }
                    println!();
                }
            }
        }
        SessionCommand::Last { path } => {
            let mut sessions = if let Some(p) = path {
                let root = std::path::PathBuf::from(p);
                find_codex_sessions_for_dir(&root, 1).await?
            } else {
                find_codex_sessions_for_current_dir(1).await?
            };
            if let Some(s) = sessions.pop() {
                println!("Last Codex session for current project:");
                println!("  id: {}", s.id);
                println!("  updated_at: {}", s.updated_at.as_deref().unwrap_or("-"));
                println!("  cwd: {}", s.cwd.as_deref().unwrap_or("-"));
                if let Some(msg) = s.first_user_message.as_deref() {
                    let msg_single = msg.replace('\n', " ");
                    println!("  first_prompt: {}", msg_single);
                }
                println!();
                println!("Resume with:");
                println!("  codex resume {}", s.id);
            } else {
                println!("No Codex sessions found under ~/.codex/sessions");
            }
        }
    }

    Ok(())
}
