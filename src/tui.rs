use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, QueueableCommand, cursor, event, terminal};

use crate::state::{ActiveRequest, ProxyState};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn pick_basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

#[derive(Debug, Clone)]
struct ActiveSessionRow {
    session_id: Option<String>,
    cwd: Option<String>,
    reasoning_effort: Option<String>,
    started_at_ms_min: u64,
    started_at_ms_max: u64,
    count: usize,
    last_method: String,
    last_path: String,
}

fn group_active_by_session(active: Vec<ActiveRequest>) -> Vec<ActiveSessionRow> {
    let mut map: HashMap<Option<String>, ActiveSessionRow> = HashMap::new();

    for req in active {
        let key = req.session_id.clone();
        let entry = map.entry(key.clone()).or_insert_with(|| ActiveSessionRow {
            session_id: key,
            cwd: req.cwd.clone(),
            reasoning_effort: req.reasoning_effort.clone(),
            started_at_ms_min: req.started_at_ms,
            started_at_ms_max: req.started_at_ms,
            count: 0,
            last_method: req.method.clone(),
            last_path: req.path.clone(),
        });

        entry.count += 1;
        entry.started_at_ms_min = entry.started_at_ms_min.min(req.started_at_ms);
        if req.started_at_ms >= entry.started_at_ms_max {
            entry.started_at_ms_max = req.started_at_ms;
            entry.last_method = req.method.clone();
            entry.last_path = req.path.clone();
        }
        if entry.cwd.is_none() {
            entry.cwd = req.cwd.clone();
        }
        if entry.reasoning_effort.is_none() {
            entry.reasoning_effort = req.reasoning_effort.clone();
        }
    }

    let mut out = map.into_values().collect::<Vec<_>>();
    out.sort_by_key(|r| r.started_at_ms_min);
    out
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        terminal::enable_raw_mode()?;
        stdout.execute(cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.execute(cursor::Show);
        let _ = terminal::disable_raw_mode();
        let _ = out.execute(LeaveAlternateScreen);
    }
}

pub async fn run_dashboard(
    state: Arc<ProxyState>,
    service_name: &'static str,
    port: u16,
    shutdown: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut stdout = std::io::stdout();

    let mut selected: usize = 0;
    let mut last_render = Instant::now() - Duration::from_secs(10);
    let mut last_lines: Option<Vec<String>> = None;
    let mut last_size: Option<(usize, usize)> = None;
    let refresh_ms = std::env::var("CODEX_HELPER_TUI_REFRESH_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(500);

    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        if last_render.elapsed() >= Duration::from_millis(refresh_ms) {
            let (frame_lines, next_selected, cols, rows) =
                build_frame(state.clone(), service_name, port, selected).await?;
            selected = next_selected;
            let size = Some((cols, rows));

            let full_redraw = last_lines
                .as_ref()
                .is_none_or(|l| l.len() != frame_lines.len())
                || last_size != size;

            if full_redraw {
                stdout.queue(cursor::MoveTo(0, 0))?;
                for (idx, line) in frame_lines.iter().enumerate() {
                    stdout.queue(cursor::MoveTo(0, idx as u16))?;
                    stdout.write_all(line.as_bytes())?;
                }
                stdout.flush()?;
                last_lines = Some(frame_lines);
                last_size = size;
            } else if let Some(prev) = last_lines.as_mut() {
                let mut changed = false;
                for (idx, (old, new)) in prev.iter_mut().zip(frame_lines.iter()).enumerate() {
                    if old != new {
                        changed = true;
                        *old = new.clone();
                        stdout.queue(cursor::MoveTo(0, idx as u16))?;
                        stdout.write_all(new.as_bytes())?;
                    }
                }
                if changed {
                    stdout.flush()?;
                }
            }
            last_render = Instant::now();
        }

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && handle_key(key, &state, &mut selected).await?
        {
            let _ = shutdown.send(true);
            return Ok(());
        }
    }
}

async fn handle_key(
    key: KeyEvent,
    state: &Arc<ProxyState>,
    selected: &mut usize,
) -> anyhow::Result<bool> {
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Ok(true);
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = selected.saturating_add(1);
        }
        KeyCode::Char('l') => apply_effort(state, *selected, "low").await?,
        KeyCode::Char('m') => apply_effort(state, *selected, "medium").await?,
        KeyCode::Char('h') => apply_effort(state, *selected, "high").await?,
        KeyCode::Char('x') => clear_effort(state, *selected).await?,
        _ => {}
    }

    Ok(false)
}

async fn apply_effort(
    state: &Arc<ProxyState>,
    selected: usize,
    effort: &str,
) -> anyhow::Result<()> {
    let active = state.list_active_requests().await;
    let grouped = group_active_by_session(active);
    let Some(row) = grouped.get(selected) else {
        return Ok(());
    };
    let Some(session_id) = row.session_id.clone() else {
        return Ok(());
    };
    state
        .set_session_effort_override(session_id, effort.to_string(), now_ms())
        .await;
    Ok(())
}

async fn clear_effort(state: &Arc<ProxyState>, selected: usize) -> anyhow::Result<()> {
    let active = state.list_active_requests().await;
    let grouped = group_active_by_session(active);
    let Some(row) = grouped.get(selected) else {
        return Ok(());
    };
    let Some(session_id) = row.session_id.as_deref() else {
        return Ok(());
    };
    state.clear_session_effort_override(session_id).await;
    Ok(())
}

async fn build_frame(
    state: Arc<ProxyState>,
    service_name: &'static str,
    port: u16,
    selected: usize,
) -> anyhow::Result<(Vec<String>, usize, usize, usize)> {
    let (cols, rows) = terminal::size()?;
    let cols = cols as usize;
    let rows = rows as usize;

    let active = state.list_active_requests().await;
    let active_grouped = group_active_by_session(active);
    let selected = if active_grouped.is_empty() {
        0
    } else if selected >= active_grouped.len() {
        active_grouped.len().saturating_sub(1)
    } else {
        selected
    };
    let recent = state.list_recent_finished(10).await;
    let overrides = state.list_session_effort_overrides().await;

    let mut lines: Vec<String> = Vec::with_capacity(rows);
    fn push_line(lines: &mut Vec<String>, cols: usize, rows: usize, s: &str) {
        if lines.len() >= rows {
            return;
        }
        let mut line = if s.chars().count() > cols {
            shorten(s, cols)
        } else {
            s.to_string()
        };
        let pad = cols.saturating_sub(line.chars().count());
        if pad > 0 {
            line.push_str(&" ".repeat(pad));
        }
        lines.push(line);
    }

    let title = format!(
        "codex-helper TUI | service={} | port={} | q 退出 | j/k 选择 | l/m/h 设置 effort | x 清除",
        service_name, port
    );
    push_line(&mut lines, cols, rows, &title);
    push_line(&mut lines, cols, rows, &"-".repeat(cols));

    push_line(&mut lines, cols, rows, "Active sessions:");
    if active_grouped.is_empty() {
        push_line(&mut lines, cols, rows, "  (none)");
    } else {
        for (idx, row) in active_grouped.iter().enumerate() {
            let marker = if idx == selected { ">" } else { " " };
            let sid = row
                .session_id
                .as_deref()
                .map(|s| shorten(s, 12))
                .unwrap_or_else(|| "-".to_string());
            let cwd = row.cwd.as_deref().map(pick_basename).unwrap_or("-");
            let effort = row.reasoning_effort.as_deref().unwrap_or("-");
            let age = now_ms().saturating_sub(row.started_at_ms_min) / 1000;
            let line = format!(
                "  {marker} [{age:>4}s] sid={sid:<12} cwd={:<16} n={:<2} effort={:<6} {} {}",
                shorten(cwd, 16),
                row.count,
                effort,
                row.last_method,
                row.last_path
            );
            push_line(&mut lines, cols, rows, &line);
        }
    }

    push_line(&mut lines, cols, rows, "");
    push_line(&mut lines, cols, rows, "Recent finished:");
    if recent.is_empty() {
        push_line(&mut lines, cols, rows, "  (none)");
    } else {
        for r in recent.iter() {
            let sid = r
                .session_id
                .as_deref()
                .map(|s| shorten(s, 12))
                .unwrap_or_else(|| "-".to_string());
            let cwd = r.cwd.as_deref().map(pick_basename).unwrap_or("-");
            let effort = r.reasoning_effort.as_deref().unwrap_or("-");
            let line = format!(
                "  [{}] {}ms sid={sid:<12} cwd={:<16} effort={:<6} {} {}",
                r.status_code,
                r.duration_ms,
                shorten(cwd, 16),
                effort,
                r.method,
                r.path
            );
            push_line(&mut lines, cols, rows, &line);
        }
    }

    push_line(&mut lines, cols, rows, "");
    push_line(
        &mut lines,
        cols,
        rows,
        &format!(
            "Session overrides (TTL={}s): {}",
            std::env::var("CODEX_HELPER_SESSION_OVERRIDE_TTL_SECS")
                .ok()
                .unwrap_or_else(|| "1800".to_string()),
            overrides.len()
        ),
    );
    if overrides.is_empty() {
        push_line(&mut lines, cols, rows, "  (none)");
    } else {
        let mut items = overrides.into_iter().collect::<Vec<_>>();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (sid, eff) in items.into_iter() {
            let line = format!("  sid={} effort={}", shorten(&sid, 24), eff);
            push_line(&mut lines, cols, rows, &line);
        }
    }

    while lines.len() < rows {
        lines.push(" ".repeat(cols));
    }
    Ok((lines, selected, cols, rows))
}
