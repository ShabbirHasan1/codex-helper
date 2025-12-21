use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio::time::{Duration, interval};

use crate::sessions;

#[derive(Debug, Clone, Serialize)]
pub struct ActiveRequest {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub service: String,
    pub method: String,
    pub path: String,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinishedRequest {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub service: String,
    pub method: String,
    pub path: String,
    pub status_code: u16,
    pub duration_ms: u64,
    pub ended_at_ms: u64,
}

#[derive(Debug, Clone)]
struct SessionEffortOverride {
    effort: String,
    #[allow(dead_code)]
    updated_at_ms: u64,
    last_seen_ms: u64,
}

/// Runtime-only state for the proxy process.
///
/// This state is intentionally not persisted across restarts.
#[derive(Debug)]
pub struct ProxyState {
    next_request_id: AtomicU64,
    session_override_ttl_ms: u64,
    session_effort_overrides: RwLock<HashMap<String, SessionEffortOverride>>,
    session_cwd_cache: RwLock<HashMap<String, Option<String>>>,
    active_requests: RwLock<HashMap<u64, ActiveRequest>>,
    recent_finished: RwLock<VecDeque<FinishedRequest>>,
}

impl ProxyState {
    pub fn new() -> Arc<Self> {
        let ttl_secs = std::env::var("CODEX_HELPER_SESSION_OVERRIDE_TTL_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(30 * 60);
        let ttl_ms = ttl_secs.saturating_mul(1000);

        Arc::new(Self {
            next_request_id: AtomicU64::new(1),
            session_override_ttl_ms: ttl_ms,
            session_effort_overrides: RwLock::new(HashMap::new()),
            session_cwd_cache: RwLock::new(HashMap::new()),
            active_requests: RwLock::new(HashMap::new()),
            recent_finished: RwLock::new(VecDeque::new()),
        })
    }

    pub async fn get_session_effort_override(&self, session_id: &str) -> Option<String> {
        let guard = self.session_effort_overrides.read().await;
        guard.get(session_id).map(|v| v.effort.clone())
    }

    pub async fn set_session_effort_override(
        &self,
        session_id: String,
        effort: String,
        now_ms: u64,
    ) {
        let mut guard = self.session_effort_overrides.write().await;
        guard.insert(
            session_id,
            SessionEffortOverride {
                effort,
                updated_at_ms: now_ms,
                last_seen_ms: now_ms,
            },
        );
    }

    pub async fn clear_session_effort_override(&self, session_id: &str) {
        let mut guard = self.session_effort_overrides.write().await;
        guard.remove(session_id);
    }

    pub async fn list_session_effort_overrides(&self) -> HashMap<String, String> {
        let guard = self.session_effort_overrides.read().await;
        guard
            .iter()
            .map(|(k, v)| (k.clone(), v.effort.clone()))
            .collect()
    }

    pub async fn touch_session_override(&self, session_id: &str, now_ms: u64) {
        let mut guard = self.session_effort_overrides.write().await;
        if let Some(v) = guard.get_mut(session_id) {
            v.last_seen_ms = now_ms;
        }
    }

    pub async fn resolve_session_cwd(&self, session_id: &str) -> Option<String> {
        {
            let guard = self.session_cwd_cache.read().await;
            if let Some(v) = guard.get(session_id) {
                return v.clone();
            }
        }

        let resolved = sessions::find_codex_session_cwd_by_id(session_id)
            .await
            .ok()
            .flatten();

        let mut guard = self.session_cwd_cache.write().await;
        guard.insert(session_id.to_string(), resolved.clone());
        resolved
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn begin_request(
        &self,
        service: &str,
        method: &str,
        path: &str,
        session_id: Option<String>,
        cwd: Option<String>,
        reasoning_effort: Option<String>,
        started_at_ms: u64,
    ) -> u64 {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let req = ActiveRequest {
            id,
            session_id,
            cwd,
            reasoning_effort,
            service: service.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            started_at_ms,
        };
        let mut guard = self.active_requests.write().await;
        guard.insert(id, req);
        id
    }

    pub async fn finish_request(
        &self,
        id: u64,
        status_code: u16,
        duration_ms: u64,
        ended_at_ms: u64,
    ) {
        let mut active = self.active_requests.write().await;
        let Some(req) = active.remove(&id) else {
            return;
        };

        let finished = FinishedRequest {
            id,
            session_id: req.session_id,
            cwd: req.cwd,
            reasoning_effort: req.reasoning_effort,
            service: req.service,
            method: req.method,
            path: req.path,
            status_code,
            duration_ms,
            ended_at_ms,
        };

        let mut recent = self.recent_finished.write().await;
        recent.push_front(finished);
        while recent.len() > 200 {
            recent.pop_back();
        }
    }

    pub async fn list_active_requests(&self) -> Vec<ActiveRequest> {
        let guard = self.active_requests.read().await;
        let mut vec = guard.values().cloned().collect::<Vec<_>>();
        vec.sort_by_key(|r| r.started_at_ms);
        vec
    }

    pub async fn list_recent_finished(&self, limit: usize) -> Vec<FinishedRequest> {
        let guard = self.recent_finished.read().await;
        guard.iter().take(limit).cloned().collect()
    }

    pub fn spawn_cleanup_task(state: Arc<Self>) {
        // Run periodically; no need to be super frequent.
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                state.prune_session_overrides().await;
            }
        });
    }

    async fn prune_session_overrides(&self) {
        if self.session_override_ttl_ms == 0 {
            return;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if now_ms < self.session_override_ttl_ms {
            return;
        }
        let cutoff = now_ms - self.session_override_ttl_ms;

        // Collect active session_ids to avoid clearing overrides for currently running requests.
        let active = self.active_requests.read().await;
        let mut active_sessions: HashMap<String, ()> = HashMap::new();
        for req in active.values() {
            if let Some(sid) = req.session_id.as_deref() {
                active_sessions.insert(sid.to_string(), ());
            }
        }

        let mut overrides = self.session_effort_overrides.write().await;
        overrides.retain(|sid, v| {
            if active_sessions.contains_key(sid) {
                return true;
            }
            v.last_seen_ms >= cutoff
        });
    }
}
