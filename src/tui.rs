use std::sync::Arc;
use std::time::Duration;

use iocraft::prelude::*;
use tokio::sync::watch;

use crate::state::{ActiveRequest, FinishedRequest, ProxyState, SessionStats};
use crate::usage::UsageMetrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Normal,
    EffortMenu,
}

#[derive(Debug, Clone, Copy)]
struct Theme {
    bg: Color,
    header_bg: Color,
    panel_bg: Color,
    border: Color,
    selected_bg: Color,
    text: Color,
    muted: Color,
    focus: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::Black,
            header_bg: Color::DarkGrey,
            panel_bg: Color::Black,
            border: Color::Grey,
            selected_bg: Color::Blue,
            text: Color::White,
            muted: Color::Grey,
            focus: Color::Cyan,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sessions,
    Recent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortChoice {
    Clear,
    Low,
    Medium,
    High,
    XHigh,
}

impl EffortChoice {
    fn label(self) -> &'static str {
        match self {
            EffortChoice::Clear => "Clear (use request value)",
            EffortChoice::Low => "low",
            EffortChoice::Medium => "medium",
            EffortChoice::High => "high",
            EffortChoice::XHigh => "xhigh",
        }
    }

    fn value(self) -> Option<&'static str> {
        match self {
            EffortChoice::Clear => None,
            EffortChoice::Low => Some("low"),
            EffortChoice::Medium => Some("medium"),
            EffortChoice::High => Some("high"),
            EffortChoice::XHigh => Some("xhigh"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRow {
    session_id: Option<String>,
    cwd: Option<String>,
    active_count: usize,
    active_started_at_ms_min: Option<u64>,
    active_last_method: Option<String>,
    active_last_path: Option<String>,
    last_status: Option<u16>,
    last_duration_ms: Option<u64>,
    last_ended_at_ms: Option<u64>,
    last_model: Option<String>,
    last_reasoning_effort: Option<String>,
    last_provider_id: Option<String>,
    last_config_name: Option<String>,
    last_usage: Option<UsageMetrics>,
    total_usage: Option<UsageMetrics>,
    turns_total: Option<u64>,
    turns_with_usage: Option<u64>,
    override_effort: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn session_sort_key(row: &SessionRow) -> u64 {
    row.last_ended_at_ms
        .unwrap_or(0)
        .max(row.active_started_at_ms_min.unwrap_or(0))
}

fn format_age(now_ms: u64, ts_ms: Option<u64>) -> String {
    let Some(ts) = ts_ms else {
        return "-".to_string();
    };
    if now_ms <= ts {
        return "0s".to_string();
    }
    let mut secs = (now_ms - ts) / 1000;
    let days = secs / 86400;
    secs %= 86400;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else if mins > 0 {
        format!("{mins}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

fn usage_line(usage: &UsageMetrics) -> String {
    format!(
        "tok in/out/rsn/ttl: {}/{}/{}/{}",
        usage.input_tokens, usage.output_tokens, usage.reasoning_tokens, usage.total_tokens
    )
}

fn tokens_short(n: i64) -> String {
    let n = n.max(0) as f64;
    if n >= 1_000_000.0 {
        format!("{:.1}m", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{:.0}", n)
    }
}

fn status_color(status: Option<u16>) -> Color {
    match status {
        Some(s) if (200..300).contains(&s) => Color::Green,
        Some(s) if (300..400).contains(&s) => Color::Cyan,
        Some(s) if (400..500).contains(&s) => Color::Yellow,
        Some(_) => Color::Red,
        None => Color::Grey,
    }
}

fn build_session_rows(
    active: Vec<ActiveRequest>,
    recent: &[FinishedRequest],
    overrides: &std::collections::HashMap<String, String>,
    stats: &std::collections::HashMap<String, SessionStats>,
) -> Vec<SessionRow> {
    use std::collections::HashMap;

    let mut map: HashMap<Option<String>, SessionRow> = HashMap::new();

    for req in active {
        let key = req.session_id.clone();
        let entry = map.entry(key.clone()).or_insert_with(|| SessionRow {
            session_id: key,
            cwd: req.cwd.clone(),
            active_count: 0,
            active_started_at_ms_min: Some(req.started_at_ms),
            active_last_method: Some(req.method.clone()),
            active_last_path: Some(req.path.clone()),
            last_status: None,
            last_duration_ms: None,
            last_ended_at_ms: None,
            last_model: req.model.clone(),
            last_reasoning_effort: req.reasoning_effort.clone(),
            last_provider_id: req.provider_id.clone(),
            last_config_name: req.config_name.clone(),
            last_usage: None,
            total_usage: None,
            turns_total: None,
            turns_with_usage: None,
            override_effort: None,
        });

        entry.active_count += 1;
        entry.active_started_at_ms_min = Some(
            entry
                .active_started_at_ms_min
                .unwrap_or(req.started_at_ms)
                .min(req.started_at_ms),
        );
        entry.active_last_method = Some(req.method);
        entry.active_last_path = Some(req.path);
        if entry.cwd.is_none() {
            entry.cwd = req.cwd;
        }
        if let Some(effort) = req.reasoning_effort {
            entry.last_reasoning_effort = Some(effort);
        }
        if entry.last_model.is_none() {
            entry.last_model = req.model;
        }
        if entry.last_provider_id.is_none() {
            entry.last_provider_id = req.provider_id;
        }
        if entry.last_config_name.is_none() {
            entry.last_config_name = req.config_name;
        }
    }

    for r in recent {
        let key = r.session_id.clone();
        let entry = map.entry(key.clone()).or_insert_with(|| SessionRow {
            session_id: key,
            cwd: r.cwd.clone(),
            active_count: 0,
            active_started_at_ms_min: None,
            active_last_method: None,
            active_last_path: None,
            last_status: None,
            last_duration_ms: None,
            last_ended_at_ms: None,
            last_model: r.model.clone(),
            last_reasoning_effort: r.reasoning_effort.clone(),
            last_provider_id: r.provider_id.clone(),
            last_config_name: r.config_name.clone(),
            last_usage: r.usage.clone(),
            total_usage: None,
            turns_total: None,
            turns_with_usage: None,
            override_effort: None,
        });

        let should_update = entry
            .last_ended_at_ms
            .map(|t| r.ended_at_ms >= t)
            .unwrap_or(true);
        if should_update {
            entry.last_status = Some(r.status_code);
            entry.last_duration_ms = Some(r.duration_ms);
            entry.last_ended_at_ms = Some(r.ended_at_ms);
            if r.reasoning_effort.is_some() {
                entry.last_reasoning_effort = r.reasoning_effort.clone();
            }
            if r.model.is_some() {
                entry.last_model = r.model.clone();
            }
            if r.provider_id.is_some() {
                entry.last_provider_id = r.provider_id.clone();
            }
            if r.config_name.is_some() {
                entry.last_config_name = r.config_name.clone();
            }
            if r.usage.is_some() {
                entry.last_usage = r.usage.clone();
            }
        }
        if entry.cwd.is_none() {
            entry.cwd = r.cwd.clone();
        }
    }

    for (sid, st) in stats.iter() {
        let key = Some(sid.clone());
        let entry = map.entry(key.clone()).or_insert_with(|| SessionRow {
            session_id: key,
            cwd: None,
            active_count: 0,
            active_started_at_ms_min: None,
            active_last_method: None,
            active_last_path: None,
            last_status: None,
            last_duration_ms: None,
            last_ended_at_ms: None,
            last_model: st.last_model.clone(),
            last_reasoning_effort: st.last_reasoning_effort.clone(),
            last_provider_id: st.last_provider_id.clone(),
            last_config_name: st.last_config_name.clone(),
            last_usage: st.last_usage.clone(),
            total_usage: Some(st.total_usage.clone()),
            turns_total: None,
            turns_with_usage: Some(st.turns_with_usage),
            override_effort: None,
        });
        entry.turns_total = Some(st.turns_total);
        if entry.last_model.is_none() {
            entry.last_model = st.last_model.clone();
        }
        if entry.last_reasoning_effort.is_none() {
            entry.last_reasoning_effort = st.last_reasoning_effort.clone();
        }
        if entry.last_provider_id.is_none() {
            entry.last_provider_id = st.last_provider_id.clone();
        }
        if entry.last_config_name.is_none() {
            entry.last_config_name = st.last_config_name.clone();
        }
        if entry.last_usage.is_none() {
            entry.last_usage = st.last_usage.clone();
        }
        if entry.total_usage.is_none() {
            entry.total_usage = Some(st.total_usage.clone());
        }
        if entry.turns_with_usage.is_none() {
            entry.turns_with_usage = Some(st.turns_with_usage);
        }
    }

    for (sid, eff) in overrides.iter() {
        let key = Some(sid.clone());
        let entry = map.entry(key.clone()).or_insert_with(|| SessionRow {
            session_id: key,
            cwd: None,
            active_count: 0,
            active_started_at_ms_min: None,
            active_last_method: None,
            active_last_path: None,
            last_status: None,
            last_duration_ms: None,
            last_ended_at_ms: None,
            last_model: None,
            last_reasoning_effort: None,
            last_provider_id: None,
            last_config_name: None,
            last_usage: None,
            total_usage: None,
            turns_total: None,
            turns_with_usage: None,
            override_effort: None,
        });
        entry.override_effort = Some(eff.clone());
    }

    let mut rows = map.into_values().collect::<Vec<_>>();
    rows.sort_by_key(|r| std::cmp::Reverse(session_sort_key(r)));
    rows
}

#[derive(Default, Props)]
struct AppProps {
    state: Option<Arc<ProxyState>>,
    service_name: Option<&'static str>,
    port: Option<u16>,
    shutdown: Option<watch::Sender<bool>>,
    shutdown_rx: Option<watch::Receiver<bool>>,
}

#[component]
fn App(props: &AppProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let mut system = hooks.use_context_mut::<SystemContext>();

    let state = props.state.clone().expect("AppProps.state is required");
    let service_name = props.service_name.unwrap_or("codex");
    let port = props.port.unwrap_or(0);
    let shutdown_tx = props
        .shutdown
        .clone()
        .expect("AppProps.shutdown is required");
    let shutdown_rx = props
        .shutdown_rx
        .clone()
        .expect("AppProps.shutdown_rx is required");

    let should_exit = hooks.use_state(|| false);
    let mode = hooks.use_state(|| UiMode::Normal);
    let focus = hooks.use_state(|| Focus::Sessions);

    let mut selected_session_idx = hooks.use_state(|| 0usize);
    let mut selected_recent_idx = hooks.use_state(|| 0usize);
    let list_scroll = hooks.use_state(|| 0i32);

    let effort_menu_idx = hooks.use_state(|| 0usize);

    let rows = hooks.use_state(Vec::<SessionRow>::new);
    let recent = hooks.use_state(Vec::<FinishedRequest>::new);
    let overrides = hooks.use_state(std::collections::HashMap::<String, String>::new);
    let stats = hooks.use_state(std::collections::HashMap::<String, SessionStats>::new);

    let refresh_ms = std::env::var("CODEX_HELPER_TUI_REFRESH_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(500);

    // Periodic refresh: pull runtime state and drive UI updates.
    {
        let state = state.clone();
        let mut rows = rows;
        let mut recent_state = recent;
        let mut overrides_state = overrides;
        let mut stats_state = stats;
        hooks.use_future(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(refresh_ms)).await;
                let active = state.list_active_requests().await;
                let recent_vec = state.list_recent_finished(200).await;
                let overrides_map = state.list_session_effort_overrides().await;
                let stats_map = state.list_session_stats().await;
                let next_rows = build_session_rows(active, &recent_vec, &overrides_map, &stats_map);

                let rows_changed = { *rows.read() != next_rows };
                if rows_changed {
                    rows.set(next_rows);
                }
                let recent_changed = { *recent_state.read() != recent_vec };
                if recent_changed {
                    recent_state.set(recent_vec);
                }
                let overrides_changed = { *overrides_state.read() != overrides_map };
                if overrides_changed {
                    overrides_state.set(overrides_map);
                }
                let stats_changed = { *stats_state.read() != stats_map };
                if stats_changed {
                    stats_state.set(stats_map);
                }
            }
        });
    }

    // Exit when the server requests shutdown.
    {
        let mut rx = shutdown_rx.clone();
        let mut should_exit = should_exit;
        hooks.use_future(async move {
            let _ = rx.changed().await;
            should_exit.set(true);
        });
    }

    let apply_effort = hooks.use_async_handler({
        let state = state.clone();
        move |(session_id, effort): (String, Option<String>)| {
            let state = state.clone();
            async move {
                let now = now_ms();
                if let Some(effort) = effort {
                    state
                        .set_session_effort_override(session_id, effort, now)
                        .await;
                } else {
                    state.clear_session_effort_override(&session_id).await;
                }
            }
        }
    });

    let request_shutdown = hooks.use_async_handler({
        let shutdown = shutdown_tx.clone();
        move |_| {
            let shutdown = shutdown.clone();
            async move {
                let _ = shutdown.send(true);
            }
        }
    });

    let rows_vec = rows.read().clone();
    let recent_vec = recent.read().clone();
    let overrides_map = overrides.read().clone();
    let stats_map = stats.read().clone();

    let selected_session_idx_clamped = if rows_vec.is_empty() {
        0
    } else {
        selected_session_idx
            .get()
            .min(rows_vec.len().saturating_sub(1))
    };
    if selected_session_idx_clamped != selected_session_idx.get() {
        selected_session_idx.set(selected_session_idx_clamped);
    }

    hooks.use_terminal_events({
        let mut should_exit = should_exit;
        let mut mode = mode;
        let mut focus = focus;
        let mut selected_session_state = selected_session_idx;
        let mut selected_recent_state = selected_recent_idx;
        let mut list_scroll = list_scroll;
        let mut effort_menu_idx = effort_menu_idx;

        move |event| match event {
            TerminalEvent::Key(KeyEvent { code, kind, .. }) if kind != KeyEventKind::Release => {
                let rows_vec = rows.read();
                let recent_vec = recent.read();

                let has_session = !rows_vec.is_empty();
                let cur_session_idx = if has_session {
                    selected_session_state
                        .get()
                        .min(rows_vec.len().saturating_sub(1))
                } else {
                    0
                };

                match mode.get() {
                    UiMode::Normal => match code {
                        KeyCode::Char('q') => {
                            should_exit.set(true);
                            request_shutdown(());
                        }
                        KeyCode::Tab => {
                            focus.set(match focus.get() {
                                Focus::Sessions => Focus::Recent,
                                Focus::Recent => Focus::Sessions,
                            });
                        }
                        KeyCode::Up | KeyCode::Char('k') => match focus.get() {
                            Focus::Sessions => {
                                if has_session {
                                    let next = cur_session_idx.saturating_sub(1);
                                    selected_session_state.set(next);
                                    let scroll = list_scroll.get();
                                    let next_scroll = (scroll).min(next as i32);
                                    list_scroll.set(next_scroll.max(0));
                                }
                            }
                            Focus::Recent => {
                                let next = selected_recent_state.get().saturating_sub(1);
                                selected_recent_state.set(next);
                            }
                        },
                        KeyCode::Down | KeyCode::Char('j') => match focus.get() {
                            Focus::Sessions => {
                                if has_session {
                                    let next = (cur_session_idx + 1).min(rows_vec.len() - 1);
                                    selected_session_state.set(next);
                                    let visible_rows = 12i32;
                                    let min_scroll =
                                        (next as i32).saturating_sub(visible_rows - 1).max(0);
                                    list_scroll.set(list_scroll.get().max(min_scroll));
                                }
                            }
                            Focus::Recent => {
                                let next = (selected_recent_state.get() + 1)
                                    .min(recent_vec.len().saturating_sub(1));
                                selected_recent_state.set(next);
                            }
                        },
                        KeyCode::Enter => {
                            if focus.get() == Focus::Sessions
                                && rows_vec
                                    .get(cur_session_idx)
                                    .and_then(|r| r.session_id.as_deref())
                                    .is_some()
                            {
                                mode.set(UiMode::EffortMenu);
                                // Preload current override selection when possible.
                                let current = rows_vec[cur_session_idx]
                                    .override_effort
                                    .as_deref()
                                    .unwrap_or("");
                                let idx = match current {
                                    "low" => 1,
                                    "medium" => 2,
                                    "high" => 3,
                                    "xhigh" => 4,
                                    _ => 0,
                                };
                                effort_menu_idx.set(idx);
                            }
                        }
                        KeyCode::Char('l')
                        | KeyCode::Char('m')
                        | KeyCode::Char('h')
                        | KeyCode::Char('X') => {
                            if focus.get() == Focus::Sessions {
                                let Some(sid) = rows_vec
                                    .get(cur_session_idx)
                                    .and_then(|r| r.session_id.clone())
                                else {
                                    return;
                                };
                                let eff = match code {
                                    KeyCode::Char('l') => Some("low"),
                                    KeyCode::Char('m') => Some("medium"),
                                    KeyCode::Char('h') => Some("high"),
                                    KeyCode::Char('X') => Some("xhigh"),
                                    _ => None,
                                }
                                .map(|s| s.to_string());
                                apply_effort((sid, eff));
                            }
                        }
                        KeyCode::Char('x') => {
                            if focus.get() == Focus::Sessions {
                                let Some(sid) = rows_vec
                                    .get(cur_session_idx)
                                    .and_then(|r| r.session_id.clone())
                                else {
                                    return;
                                };
                                apply_effort((sid, None));
                            }
                        }
                        _ => {}
                    },
                    UiMode::EffortMenu => match code {
                        KeyCode::Esc => mode.set(UiMode::Normal),
                        KeyCode::Up | KeyCode::Char('k') => {
                            effort_menu_idx.set(effort_menu_idx.get().saturating_sub(1));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            effort_menu_idx.set((effort_menu_idx.get() + 1).min(4));
                        }
                        KeyCode::Enter => {
                            let Some(sid) = rows_vec
                                .get(cur_session_idx)
                                .and_then(|r| r.session_id.clone())
                            else {
                                mode.set(UiMode::Normal);
                                return;
                            };
                            let choice = match effort_menu_idx.get() {
                                1 => EffortChoice::Low,
                                2 => EffortChoice::Medium,
                                3 => EffortChoice::High,
                                4 => EffortChoice::XHigh,
                                _ => EffortChoice::Clear,
                            };
                            apply_effort((sid, choice.value().map(|s| s.to_string())));
                            mode.set(UiMode::Normal);
                        }
                        _ => {}
                    },
                }
            }
            _ => {}
        }
    });

    if should_exit.get() {
        system.exit();
        return element!(View);
    }

    let (width, height) = hooks.use_terminal_size();
    let theme = Theme::default();
    let title = format!(
        "codex-helper | {}:{} | Tab 切换焦点 | Enter effort 菜单 | l/m/h/X 快速设置 | x 清除 | q 退出",
        service_name, port
    );

    let selected_row = rows_vec.get(selected_session_idx_clamped);
    let selected_sid = selected_row
        .and_then(|r| r.session_id.as_deref())
        .unwrap_or("-");
    let selected_cwd = selected_row
        .and_then(|r| r.cwd.as_deref())
        .map(|s| shorten(s, 48))
        .unwrap_or_else(|| "-".to_string());

    let selected_override = selected_row
        .and_then(|r| r.override_effort.as_deref())
        .unwrap_or("-");
    let selected_effort = selected_row
        .and_then(|r| r.override_effort.as_deref())
        .or_else(|| selected_row.and_then(|r| r.last_reasoning_effort.as_deref()))
        .unwrap_or("-");
    let selected_model = selected_row
        .and_then(|r| r.last_model.as_deref())
        .unwrap_or("-");
    let selected_provider = selected_row
        .and_then(|r| r.last_provider_id.as_deref())
        .unwrap_or("-");
    let selected_config = selected_row
        .and_then(|r| r.last_config_name.as_deref())
        .unwrap_or("-");
    let selected_turns_total = selected_row
        .and_then(|r| r.turns_total)
        .or_else(|| {
            selected_row
                .and_then(|r| r.session_id.as_ref())
                .and_then(|sid| stats_map.get(sid).map(|s| s.turns_total))
        })
        .unwrap_or(0);
    let now = now_ms();
    let selected_active_age = if selected_row.map(|r| r.active_count).unwrap_or(0) > 0 {
        format_age(now, selected_row.and_then(|r| r.active_started_at_ms_min))
    } else {
        "-".to_string()
    };
    let selected_last_age = format_age(now, selected_row.and_then(|r| r.last_ended_at_ms));
    let selected_last_usage = selected_row
        .and_then(|r| r.last_usage.clone())
        .map(|u| usage_line(&u))
        .unwrap_or_else(|| "tok in/out/rsn/ttl: -".to_string());
    let selected_total_usage = selected_row
        .and_then(|r| r.total_usage.clone())
        .filter(|u| u.total_tokens > 0)
        .map(|u| {
            format!(
                "tok sum in/out/rsn/ttl: {}/{}/{}/{}",
                u.input_tokens, u.output_tokens, u.reasoning_tokens, u.total_tokens
            )
        })
        .unwrap_or_else(|| "tok sum in/out/rsn/ttl: -".to_string());

    let menu_open = mode.get() == UiMode::EffortMenu;

    let mut recent_for_selected: Vec<FinishedRequest> = Vec::new();
    if let Some(sid) = selected_row.and_then(|r| r.session_id.as_deref()) {
        for r in recent_vec.iter() {
            if r.session_id.as_deref() == Some(sid) {
                recent_for_selected.push(r.clone());
            }
            if recent_for_selected.len() >= 12 {
                break;
            }
        }
    }

    let selected_recent_idx_clamped = if recent_for_selected.is_empty() {
        0
    } else {
        selected_recent_idx
            .get()
            .min(recent_for_selected.len().saturating_sub(1))
    };
    if selected_recent_idx_clamped != selected_recent_idx.get() {
        selected_recent_idx.set(selected_recent_idx_clamped);
    }

    let recent_children: Vec<AnyElement<'static>> = if recent_for_selected.is_empty() {
        vec![element!(Text(content: "  (none)", color: theme.muted)).into()]
    } else {
        recent_for_selected
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let selected = focus.get() == Focus::Recent && idx == selected_recent_idx_clamped;
                let bg = if selected {
                    Some(theme.selected_bg)
                } else {
                    None
                };
                let st_color = status_color(Some(r.status_code));
                element! {
                    View(
                        background_color: bg,
                        padding_left: 1,
                        padding_right: 1,
                        flex_direction: FlexDirection::Row,
                    ) {
                        Text(
                            content: format!("  [{}] ", r.status_code),
                            wrap: TextWrap::NoWrap,
                            color: if selected { theme.text } else { st_color },
                            weight: if selected { Weight::Bold } else { Weight::Normal },
                        )
                        Text(
                            content: format!("{:>5}ms ", r.duration_ms),
                            wrap: TextWrap::NoWrap,
                            color: if selected { theme.text } else { theme.muted },
                        )
                        Text(
                            content: format!("{} {}", r.method, shorten(r.path.as_str(), 48)),
                            wrap: TextWrap::NoWrap,
                            color: if selected { theme.text } else { theme.text },
                        )
                    }
                }
                .into()
            })
            .collect()
    };

    let overrides_children: Vec<AnyElement<'static>> = overrides_map
        .iter()
        .take(8)
        .map(|(sid, eff)| {
            element! {
                Text(
                    content: format!("  {} => {}", shorten(sid, 24), eff),
                    color: theme.muted
                )
            }
            .into()
        })
        .collect();

    element! {
        View(
            width,
            height,
            background_color: theme.bg,
            flex_direction: FlexDirection::Column,
        ) {
            View(
                border_style: BorderStyle::Single,
                border_edges: Edges::Bottom,
                border_color: theme.border,
                background_color: theme.header_bg,
                padding_left: 1,
                padding_right: 1,
            ) {
                Text(content: title, weight: Weight::Bold, color: theme.text)
            }

            View(
                flex_grow: 1.0,
                flex_direction: FlexDirection::Row,
            ) {
                // Left: sessions list
                View(
                    width: 48,
                    border_style: BorderStyle::Round,
                    border_color: theme.border,
                    background_color: theme.panel_bg,
                    flex_direction: FlexDirection::Column,
                ) {
                    View(padding_left: 1, padding_right: 1, padding_top: 1, padding_bottom: 1) {
                        Text(
                            content: format!(
                                "Sessions ({}){}",
                                rows_vec.len(),
                                if focus.get() == Focus::Sessions { " [focus]" } else { "" }
                            ),
                            color: if focus.get() == Focus::Sessions {
                                theme.focus
                            } else {
                                theme.muted
                            },
                            weight: Weight::Bold,
                        )
                    }

                    View(
                        flex_grow: 1.0,
                        overflow: Overflow::Hidden,
                        padding_left: 1,
                        padding_right: 1,
                    ) {
                        View(
                            position: Position::Absolute,
                            top: -list_scroll.get(),
                            flex_direction: FlexDirection::Column,
                        ) {
                            #(rows_vec.iter().enumerate().map(|(idx, row)| {
                                let selected = idx == selected_session_idx_clamped;
                                let bg = if selected { Some(theme.selected_bg) } else { None };
                                let sid = row.session_id.as_deref().map(|s| shorten(s, 10)).unwrap_or_else(|| "-".to_string());
                                let cwd = row.cwd.as_deref().map(basename).unwrap_or("-");
                                let effort = row
                                    .override_effort
                                    .as_deref()
                                    .or(row.last_reasoning_effort.as_deref())
                                    .unwrap_or("-");
                                let last_status = row.last_status.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
                                let st_color = status_color(row.last_status);
                                let active_n = row.active_count;
                                let active_color = if active_n > 0 { Color::Green } else { theme.muted };
                                let model = row.last_model.as_deref().map(|s| shorten(s, 10)).unwrap_or_else(|| "-".to_string());
                                let provider = row.last_provider_id.as_deref().map(|s| shorten(s, 8)).unwrap_or_else(|| "-".to_string());
                                let turns = row.turns_total.unwrap_or(0);
                                let tok_sum = row.total_usage.as_ref().map(|u| tokens_short(u.total_tokens)).unwrap_or_else(|| "-".to_string());
                                element! {
                                    View(
                                        background_color: bg,
                                        padding_left: 1,
                                        padding_right: 1,
                                        flex_direction: FlexDirection::Row,
                                    ) {
                                        Text(
                                            content: format!("{:>3} ", idx),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("{:<10} ", shorten(cwd, 10)),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.text },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("{:<12} ", sid),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.text },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("n={:<2} ", active_n),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { active_color },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("st={:<3} ", last_status),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { st_color },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("mdl={:<10} ", model),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("t={:<4} ", turns),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("pv={:<8} ", provider),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("tok={} ", tok_sum),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                        Text(
                                            content: format!("eff={}", shorten(effort, 6)),
                                            wrap: TextWrap::NoWrap,
                                            color: if selected { theme.text } else { theme.muted },
                                            weight: if selected { Weight::Bold } else { Weight::Normal },
                                        )
                                    }
                                }
                            }))
                        }
                    }
                }

                // Right: details / recent
                View(
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    padding_left: 2,
                    padding_right: 2,
                    padding_top: 1,
                ) {
                    View(
                        border_style: BorderStyle::Round,
                        border_color: theme.border,
                        background_color: theme.panel_bg,
                        padding_left: 1,
                        padding_right: 1,
                        padding_top: 1,
                        padding_bottom: 1,
                        margin_bottom: 1,
                        flex_direction: FlexDirection::Column,
                    ) {
                        Text(content: "Selected session", weight: Weight::Bold, color: theme.text)
                        Text(content: format!("sid: {}", selected_sid), color: theme.muted)
                        Text(content: format!("cwd: {}", selected_cwd), color: theme.muted)
                        Text(content: format!("effort: {}  (override: {})", selected_effort, selected_override), color: theme.muted)
                        Text(content: format!("model: {}", selected_model), color: theme.muted)
                        Text(content: format!("provider: {}  (config: {})", selected_provider, selected_config), color: theme.muted)
                        Text(content: format!("turns: {}", selected_turns_total), color: theme.muted)
                        Text(content: format!("active age: {}  last done: {}", selected_active_age, selected_last_age), color: theme.muted)
                        Text(content: selected_last_usage, color: theme.muted)
                        Text(content: selected_total_usage, color: theme.muted)
                    }

                    View(
                        border_style: BorderStyle::Round,
                        border_color: theme.border,
                        background_color: theme.panel_bg,
                        padding_left: 1,
                        padding_right: 1,
                        padding_top: 1,
                        padding_bottom: 1,
                        margin_bottom: 1,
                        flex_direction: FlexDirection::Column,
                    ) {
                        Text(
                            content: format!(
                                "Recent finished{}",
                                if focus.get() == Focus::Recent { " [focus]" } else { "" }
                            ),
                            weight: Weight::Bold,
                            color: if focus.get() == Focus::Recent {
                                theme.focus
                            } else {
                                theme.text
                            },
                        )
                        #(recent_children)
                    }

                    View(
                        border_style: BorderStyle::Round,
                        border_color: theme.border,
                        background_color: theme.panel_bg,
                        padding_left: 1,
                        padding_right: 1,
                        padding_top: 1,
                        padding_bottom: 1,
                        flex_direction: FlexDirection::Column,
                    ) {
                        Text(
                            content: format!("Session overrides ({})", overrides_map.len()),
                            weight: Weight::Bold,
                            color: theme.text,
                        )
                        #(overrides_children)
                    }
                }

                // Modal: effort menu
                #(
                    menu_open.then(|| element! {
                        View(
                            position: Position::Absolute,
                            top: 4,
                            left: 10,
                            background_color: theme.panel_bg,
                            border_style: BorderStyle::Round,
                            border_color: theme.focus,
                            padding_left: 2,
                            padding_right: 2,
                            padding_top: 1,
                            padding_bottom: 1,
                            flex_direction: FlexDirection::Column,
                        ) {
                            Text(content: "Set reasoning.effort", weight: Weight::Bold, color: theme.text)
                            Text(content: "Up/Down 选择，Enter 应用，Esc 取消", color: theme.muted)
                            View(margin_top: 1, flex_direction: FlexDirection::Column) {
                                #( [EffortChoice::Clear, EffortChoice::Low, EffortChoice::Medium, EffortChoice::High, EffortChoice::XHigh]
                                    .into_iter()
                                    .enumerate()
                                    .map(|(idx, choice)| {
                                        let selected = idx == effort_menu_idx.get();
                                        let bg = if selected { Some(theme.selected_bg) } else { None };
                                        element! {
                                            View(background_color: bg, padding_left: 1, padding_right: 1) {
                                                Text(
                                                    content: choice.label(),
                                                    color: if selected { theme.text } else { theme.muted },
                                                    wrap: TextWrap::NoWrap,
                                                    weight: if selected { Weight::Bold } else { Weight::Normal },
                                                )
                                            }
                                        }
                                    })
                                )
                            }
                        }
                    }).into_iter()
                )
            }
        }
    }
}

pub async fn run_dashboard(
    state: Arc<ProxyState>,
    service_name: &'static str,
    port: u16,
    shutdown: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut el = element! {
        App(
            state: Some(state.clone()),
            service_name: Some(service_name),
            port: Some(port),
            shutdown: Some(shutdown),
            shutdown_rx: Some(shutdown_rx),
        )
    };
    el.fullscreen().await?;
    Ok(())
}
