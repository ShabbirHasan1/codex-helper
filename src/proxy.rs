use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::Query;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::{any, get};
use futures_util::TryStreamExt;
use rand::Rng;
use reqwest::Client;
use std::sync::OnceLock;
use tokio::time::sleep;
use tracing::{info, instrument, warn};

use crate::config::{ProxyConfig, RetryConfig, ServiceConfigManager};
use crate::filter::RequestFilter;
use crate::lb::{LbState, LoadBalancer, SelectedUpstream};
use crate::logging::{
    AuthResolutionLog, BodyPreview, HeaderEntry, HttpDebugLog, RetryInfo, http_debug_options,
    http_warn_options, log_request_with_debug, make_body_preview, should_include_http_debug,
    should_include_http_warn, should_log_request_body_preview,
};
use crate::state::{ActiveRequest, FinishedRequest, ProxyState};
use crate::usage::extract_usage_from_bytes;
use crate::usage_providers;

fn read_json_file(path: &std::path::Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

fn codex_auth_json_value(key: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<serde_json::Value>> = std::sync::OnceLock::new();
    let v = CACHE.get_or_init(|| read_json_file(&crate::config::codex_auth_path()));
    let obj = v.as_ref()?.as_object()?;
    obj.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn claude_settings_env_value(key: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<serde_json::Value>> = std::sync::OnceLock::new();
    let v = CACHE.get_or_init(|| read_json_file(&crate::config::claude_settings_path()));
    let obj = v.as_ref()?.as_object()?;
    let env_obj = obj.get("env")?.as_object()?;
    env_obj
        .get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

fn resolve_auth_token_with_source(
    service_name: &str,
    auth: &crate::config::UpstreamAuth,
    client_has_auth: bool,
) -> (Option<String>, String) {
    if let Some(token) = auth.auth_token.as_deref()
        && !token.trim().is_empty()
    {
        return (Some(token.to_string()), "inline".to_string());
    }

    if let Some(env_name) = auth.auth_token_env.as_deref()
        && !env_name.trim().is_empty()
    {
        if let Ok(v) = std::env::var(env_name)
            && !v.trim().is_empty()
        {
            return (Some(v), format!("env:{env_name}"));
        }

        let file_value = match service_name {
            "codex" => codex_auth_json_value(env_name),
            "claude" => claude_settings_env_value(env_name),
            _ => None,
        };
        if let Some(v) = file_value
            && !v.trim().is_empty()
        {
            let src = match service_name {
                "codex" => format!("codex_auth_json:{env_name}"),
                "claude" => format!("claude_settings_env:{env_name}"),
                _ => format!("file:{env_name}"),
            };
            return (Some(v), src);
        }

        if client_has_auth {
            return (None, format!("client_passthrough (missing_env:{env_name})"));
        }
        return (None, format!("missing_env:{env_name}"));
    }

    if client_has_auth {
        (None, "client_passthrough".to_string())
    } else {
        (None, "none".to_string())
    }
}

fn resolve_api_key_with_source(
    service_name: &str,
    auth: &crate::config::UpstreamAuth,
    client_has_x_api_key: bool,
) -> (Option<String>, String) {
    if let Some(key) = auth.api_key.as_deref()
        && !key.trim().is_empty()
    {
        return (Some(key.to_string()), "inline".to_string());
    }

    if let Some(env_name) = auth.api_key_env.as_deref()
        && !env_name.trim().is_empty()
    {
        if let Ok(v) = std::env::var(env_name)
            && !v.trim().is_empty()
        {
            return (Some(v), format!("env:{env_name}"));
        }

        let file_value = match service_name {
            "codex" => codex_auth_json_value(env_name),
            "claude" => claude_settings_env_value(env_name),
            _ => None,
        };
        if let Some(v) = file_value
            && !v.trim().is_empty()
        {
            let src = match service_name {
                "codex" => format!("codex_auth_json:{env_name}"),
                "claude" => format!("claude_settings_env:{env_name}"),
                _ => format!("file:{env_name}"),
            };
            return (Some(v), src);
        }

        if client_has_x_api_key {
            return (None, format!("client_passthrough (missing_env:{env_name})"));
        }
        return (None, format!("missing_env:{env_name}"));
    }

    if client_has_x_api_key {
        (None, "client_passthrough".to_string())
    } else {
        (None, "none".to_string())
    }
}

fn header_value_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn looks_like_cloudflare_challenge_html(headers: &HeaderMap, body: &[u8]) -> bool {
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !ct.starts_with("text/html") {
        return false;
    }
    contains_bytes(body, b"__CF$cv$params")
        || contains_bytes(body, b"/cdn-cgi/")
        || contains_bytes(body, b"challenge-platform")
        || contains_bytes(body, b"cf-chl-")
}

fn classify_upstream_response(
    status_code: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> (Option<String>, Option<String>, Option<String>) {
    let cf_ray = header_value_str(headers, "cf-ray");
    let server = header_value_str(headers, "server")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let looks_cf = server.contains("cloudflare") || cf_ray.is_some();

    if looks_cf && status_code == 524 {
        return (
            Some("cloudflare_timeout".to_string()),
            Some("Cloudflare 524：通常表示源站在规定时间内未返回响应；建议检查上游服务耗时、首包是否及时输出（SSE），以及 Cloudflare/WAF 规则。".to_string()),
            cf_ray,
        );
    }

    if looks_like_cloudflare_challenge_html(headers, body) {
        return (
            Some("cloudflare_challenge".to_string()),
            Some("检测到 Cloudflare/WAF 拦截页（text/html + cdn-cgi/challenge 标记）；通常不是 API JSON 错误，请检查 WAF 规则、UA/头部、以及是否需要放行该路径。".to_string()),
            cf_ray,
        );
    }

    (None, None, cf_ray)
}

fn is_hop_by_hop_header(name_lower: &str) -> bool {
    matches!(
        name_lower,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn hop_by_hop_connection_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for value in headers.get_all("connection").iter() {
        let Ok(s) = value.to_str() else {
            continue;
        };
        for token in s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
            out.push(token.to_ascii_lowercase());
        }
    }
    out
}

fn filter_request_headers(src: &HeaderMap) -> HeaderMap {
    let extra = hop_by_hop_connection_tokens(src);
    let mut out = HeaderMap::new();
    for (name, value) in src.iter() {
        let name_lower = name.as_str().to_ascii_lowercase();
        if name_lower == "host"
            || name_lower == "content-length"
            || is_hop_by_hop_header(&name_lower)
        {
            continue;
        }
        if extra.iter().any(|t| t == &name_lower) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn filter_response_headers(src: &HeaderMap) -> HeaderMap {
    let extra = hop_by_hop_connection_tokens(src);
    let mut out = HeaderMap::new();
    for (name, value) in src.iter() {
        let name_lower = name.as_str().to_ascii_lowercase();
        // reqwest 可能会自动解压响应体；为避免 content-length/content-encoding 与实际 body 不一致，这里不透传它们。
        if is_hop_by_hop_header(&name_lower)
            || name_lower == "content-length"
            || name_lower == "content-encoding"
        {
            continue;
        }
        if extra.iter().any(|t| t == &name_lower) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn header_map_to_entries(headers: &HeaderMap) -> Vec<HeaderEntry> {
    fn is_sensitive(name_lower: &str) -> bool {
        matches!(
            name_lower,
            "authorization"
                | "proxy-authorization"
                | "cookie"
                | "set-cookie"
                | "x-api-key"
                | "x-forwarded-api-key"
                | "x-goog-api-key"
        )
    }

    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let name_lower = name.as_str().to_ascii_lowercase();
        let v = if is_sensitive(name_lower.as_str()) {
            "[REDACTED]".to_string()
        } else {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        };
        out.push(HeaderEntry {
            name: name.as_str().to_string(),
            value: v,
        });
    }
    out
}

#[derive(Clone)]
struct HttpDebugBase {
    debug_max_body_bytes: usize,
    warn_max_body_bytes: usize,
    request_body_len: usize,
    upstream_request_body_len: usize,
    client_uri: String,
    target_url: String,
    client_headers: Vec<HeaderEntry>,
    upstream_request_headers: Vec<HeaderEntry>,
    auth_resolution: Option<AuthResolutionLog>,
    client_body_debug: Option<BodyPreview>,
    upstream_request_body_debug: Option<BodyPreview>,
    client_body_warn: Option<BodyPreview>,
    upstream_request_body_warn: Option<BodyPreview>,
}

fn warn_http_debug(status_code: u16, http_debug: &HttpDebugLog) {
    let max_chars = 2048usize;
    let Ok(mut json) = serde_json::to_string(http_debug) else {
        return;
    };
    if json.chars().count() > max_chars {
        json = json.chars().take(max_chars).collect::<String>() + "...[TRUNCATED_FOR_LOG]";
    }
    warn!("upstream non-2xx http_debug={json} status_code={status_code}");
}

#[derive(Clone)]
struct RetryOptions {
    max_attempts: u32,
    base_backoff_ms: u64,
    max_backoff_ms: u64,
    jitter_ms: u64,
    retry_status_ranges: Vec<(u16, u16)>,
    retry_error_classes: Vec<String>,
    cloudflare_challenge_cooldown_secs: u64,
    cloudflare_timeout_cooldown_secs: u64,
    transport_cooldown_secs: u64,
}

fn parse_status_ranges(spec: &str) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for raw in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Some((a, b)) = raw.split_once('-') {
            let (Ok(start), Ok(end)) = (a.trim().parse::<u16>(), b.trim().parse::<u16>()) else {
                continue;
            };
            out.push((start.min(end), start.max(end)));
        } else if let Ok(code) = raw.parse::<u16>() {
            out.push((code, code));
        }
    }
    out
}

fn retry_options(cfg: &RetryConfig) -> RetryOptions {
    let max_attempts = std::env::var("CODEX_HELPER_RETRY_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(cfg.max_attempts)
        .min(8);
    let base_backoff_ms = std::env::var("CODEX_HELPER_RETRY_BACKOFF_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(cfg.backoff_ms);
    let max_backoff_ms = std::env::var("CODEX_HELPER_RETRY_BACKOFF_MAX_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(cfg.backoff_max_ms);
    let jitter_ms = std::env::var("CODEX_HELPER_RETRY_JITTER_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(cfg.jitter_ms);
    let retry_status_ranges = std::env::var("CODEX_HELPER_RETRY_ON_STATUS")
        .ok()
        .map(|s| parse_status_ranges(&s))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| parse_status_ranges(cfg.on_status.as_str()));
    let retry_error_classes = std::env::var("CODEX_HELPER_RETRY_ON_CLASS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim())
                .filter(|x| !x.is_empty())
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| cfg.on_class.clone());
    let cloudflare_challenge_cooldown_secs =
        std::env::var("CODEX_HELPER_RETRY_CLOUDFLARE_CHALLENGE_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(cfg.cloudflare_challenge_cooldown_secs);
    let cloudflare_timeout_cooldown_secs =
        std::env::var("CODEX_HELPER_RETRY_CLOUDFLARE_TIMEOUT_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(cfg.cloudflare_timeout_cooldown_secs);
    let transport_cooldown_secs = std::env::var("CODEX_HELPER_RETRY_TRANSPORT_COOLDOWN_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(cfg.transport_cooldown_secs);

    RetryOptions {
        max_attempts,
        base_backoff_ms,
        max_backoff_ms,
        jitter_ms,
        retry_status_ranges,
        retry_error_classes,
        cloudflare_challenge_cooldown_secs,
        cloudflare_timeout_cooldown_secs,
        transport_cooldown_secs,
    }
}

fn retry_info_for_chain(chain: &[String]) -> Option<RetryInfo> {
    if chain.len() <= 1 {
        return None;
    }
    Some(RetryInfo {
        attempts: chain.len() as u32,
        upstream_chain: chain.to_vec(),
    })
}

fn should_retry_status(opt: &RetryOptions, status_code: u16) -> bool {
    opt.retry_status_ranges
        .iter()
        .any(|(a, b)| status_code >= *a && status_code <= *b)
}

fn should_retry_class(opt: &RetryOptions, class: Option<&str>) -> bool {
    let Some(c) = class else {
        return false;
    };
    opt.retry_error_classes.iter().any(|x| x == c)
}

fn retry_after_ms(headers: &HeaderMap, opt: &RetryOptions) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let seconds = raw.parse::<u64>().ok()?;
    let ms = seconds.saturating_mul(1000);
    let cap = opt.max_backoff_ms.max(opt.base_backoff_ms);
    Some(ms.min(cap))
}

async fn backoff_sleep(opt: &RetryOptions, attempt_index: u32) {
    if opt.base_backoff_ms == 0 {
        return;
    }
    let pow = 1u64 << attempt_index.min(20);
    let base = opt.base_backoff_ms.saturating_mul(pow);
    let capped = base.min(opt.max_backoff_ms.max(opt.base_backoff_ms));
    let jitter = if opt.jitter_ms == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..=opt.jitter_ms)
    };
    sleep(std::time::Duration::from_millis(
        capped.saturating_add(jitter),
    ))
    .await;
}

async fn retry_sleep(opt: &RetryOptions, attempt_index: u32, resp_headers: &HeaderMap) {
    if let Some(mut ms) = retry_after_ms(resp_headers, opt) {
        if opt.jitter_ms > 0 {
            let jitter = rand::thread_rng().gen_range(0..=opt.jitter_ms);
            let cap = opt.max_backoff_ms.max(opt.base_backoff_ms);
            ms = ms.saturating_add(jitter).min(cap);
        }
        if ms > 0 {
            sleep(std::time::Duration::from_millis(ms)).await;
        }
        return;
    }
    backoff_sleep(opt, attempt_index).await;
}

/// Generic proxy service; currently used by both Codex and Claude.
#[derive(Clone)]
pub struct ProxyService {
    pub client: Client,
    pub config: Arc<ProxyConfig>,
    pub service_name: &'static str,
    lb_states: Arc<Mutex<HashMap<String, LbState>>>,
    filter: RequestFilter,
    state: Arc<ProxyState>,
}

impl ProxyService {
    pub fn new(
        client: Client,
        config: Arc<ProxyConfig>,
        service_name: &'static str,
        lb_states: Arc<Mutex<HashMap<String, LbState>>>,
    ) -> Self {
        let state = ProxyState::new();
        ProxyState::spawn_cleanup_task(state.clone());
        Self {
            client,
            config,
            service_name,
            lb_states,
            filter: RequestFilter::new(),
            state,
        }
    }

    fn service_manager(&self) -> &ServiceConfigManager {
        match self.service_name {
            "codex" => &self.config.codex,
            "claude" => &self.config.claude,
            _ => &self.config.codex,
        }
    }

    async fn effective_config_name(&self, session_id: Option<&str>) -> Option<String> {
        if let Some(sid) = session_id
            && let Some(name) = self.state.get_session_config_override(sid).await
            && !name.trim().is_empty()
        {
            return Some(name);
        }
        if let Some(name) = self.state.get_global_config_override().await
            && !name.trim().is_empty()
        {
            return Some(name);
        }
        let mgr = self.service_manager();
        if let Some(name) = mgr.active.as_deref()
            && !name.trim().is_empty()
        {
            return Some(name.to_string());
        }
        mgr.active_config().map(|svc| svc.name.clone())
    }

    async fn lb_for_request(&self, session_id: Option<&str>) -> Option<LoadBalancer> {
        let mgr = self.service_manager();
        let name = self.effective_config_name(session_id).await?;
        let svc = mgr
            .configs
            .get(&name)
            .or_else(|| mgr.active_config())
            .cloned()?;
        Some(LoadBalancer::new(Arc::new(svc), self.lb_states.clone()))
    }

    fn build_target(
        &self,
        upstream: &SelectedUpstream,
        uri: &Uri,
    ) -> Result<(reqwest::Url, HeaderMap)> {
        let base = upstream.upstream.base_url.trim_end_matches('/').to_string();

        let base_url = reqwest::Url::parse(&base)
            .map_err(|e| anyhow!("invalid upstream base_url {base}: {e}"))?;
        let base_path = base_url.path().trim_end_matches('/').to_string();

        let mut path = uri.path().to_string();
        if !base_path.is_empty()
            && base_path != "/"
            && (path == base_path || path.starts_with(&format!("{base_path}/")))
        {
            // If the incoming request path already contains the base_url path prefix,
            // strip it to avoid double-prefixing (e.g. base_url=/v1 and request=/v1/responses).
            let rest = &path[base_path.len()..];
            path = if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            };
            if !path.starts_with('/') {
                path = format!("/{path}");
            }
        }
        let path_and_query = if let Some(q) = uri.query() {
            format!("{path}?{q}")
        } else {
            path
        };

        let full = format!("{base}{path_and_query}");
        let url =
            reqwest::Url::parse(&full).map_err(|e| anyhow!("invalid upstream url {full}: {e}"))?;

        // ensure query preserved (Url::parse already includes it)
        let headers = HeaderMap::new();
        Ok((url, headers))
    }

    pub fn state_handle(&self) -> Arc<ProxyState> {
        self.state.clone()
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    header_str(headers, "session_id")
        .or_else(|| header_str(headers, "conversation_id"))
        .map(|s| s.to_string())
}

fn extract_reasoning_effort_from_request_body(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

fn extract_model_from_request_body(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

fn apply_reasoning_effort_override(body: &[u8], effort: &str) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let reasoning = v.get_mut("reasoning").and_then(|r| r.as_object_mut());
    if let Some(obj) = reasoning {
        obj.insert(
            "effort".to_string(),
            serde_json::Value::String(effort.to_string()),
        );
    } else {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "effort".to_string(),
            serde_json::Value::String(effort.to_string()),
        );
        v.as_object_mut()?
            .insert("reasoning".to_string(), serde_json::Value::Object(obj));
    }
    serde_json::to_vec(&v).ok()
}

#[instrument(skip_all, fields(service = %proxy.service_name))]
pub async fn handle_proxy(
    proxy: ProxyService,
    req: Request<Body>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let start = Instant::now();
    let started_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let (parts, body) = req.into_parts();
    let uri = parts.uri;
    let method = parts.method;
    let client_headers = parts.headers;
    let client_headers_entries_cache: OnceLock<Vec<HeaderEntry>> = OnceLock::new();

    let session_id = extract_session_id(&client_headers);

    let lb = match proxy.lb_for_request(session_id.as_deref()).await {
        Some(lb) => lb,
        None => {
            let dur = start.elapsed().as_millis() as u64;
            let status = StatusCode::BAD_GATEWAY;
            let client_headers_entries = client_headers_entries_cache
                .get_or_init(|| header_map_to_entries(&client_headers))
                .clone();
            let http_debug = if should_include_http_warn(status.as_u16()) {
                Some(HttpDebugLog {
                    request_body_len: None,
                    upstream_request_body_len: None,
                    upstream_headers_ms: None,
                    upstream_first_chunk_ms: None,
                    upstream_body_read_ms: None,
                    upstream_error_class: Some("no_active_upstream_config".to_string()),
                    upstream_error_hint: Some(
                        "未找到任何可用的上游配置（active_config 为空或 upstreams 为空）。"
                            .to_string(),
                    ),
                    upstream_cf_ray: None,
                    client_uri: uri.to_string(),
                    target_url: "-".to_string(),
                    client_headers: client_headers_entries,
                    upstream_request_headers: Vec::new(),
                    auth_resolution: None,
                    client_body: None,
                    upstream_request_body: None,
                    upstream_response_headers: None,
                    upstream_response_body: None,
                    upstream_error: Some("no active upstream config".to_string()),
                })
            } else {
                None
            };
            log_request_with_debug(
                proxy.service_name,
                method.as_str(),
                uri.path(),
                status.as_u16(),
                dur,
                "-",
                "-",
                session_id.clone(),
                None,
                None,
                None,
                None,
                http_debug,
            );
            return Err((status, "no active upstream config".to_string()));
        }
    };
    let client_content_type = client_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok());

    // Detect streaming (SSE).
    let is_stream = client_headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    let path = uri.path();
    let is_responses_path = path.ends_with("/responses");
    let is_user_turn = method == Method::POST && is_responses_path;
    let is_codex_service = proxy.service_name == "codex";

    let cwd = if let Some(id) = session_id.as_deref() {
        proxy.state.resolve_session_cwd(id).await
    } else {
        None
    };
    if let Some(id) = session_id.as_deref() {
        proxy.state.touch_session_override(id, started_at_ms).await;
        proxy
            .state
            .touch_session_config_override(id, started_at_ms)
            .await;
    }

    // Read request body and apply filters.
    let raw_body = match to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            let dur = start.elapsed().as_millis() as u64;
            let status = StatusCode::BAD_REQUEST;
            let err_str = e.to_string();
            let client_headers_entries = client_headers_entries_cache
                .get_or_init(|| header_map_to_entries(&client_headers))
                .clone();
            let http_debug = if should_include_http_warn(status.as_u16()) {
                Some(HttpDebugLog {
                    request_body_len: None,
                    upstream_request_body_len: None,
                    upstream_headers_ms: None,
                    upstream_first_chunk_ms: None,
                    upstream_body_read_ms: None,
                    upstream_error_class: Some("client_body_read_error".to_string()),
                    upstream_error_hint: Some(
                        "读取客户端请求 body 失败（可能超过大小限制或连接中断）。".to_string(),
                    ),
                    upstream_cf_ray: None,
                    client_uri: uri.to_string(),
                    target_url: "-".to_string(),
                    client_headers: client_headers_entries,
                    upstream_request_headers: Vec::new(),
                    auth_resolution: None,
                    client_body: None,
                    upstream_request_body: None,
                    upstream_response_headers: None,
                    upstream_response_body: None,
                    upstream_error: Some(err_str.clone()),
                })
            } else {
                None
            };
            log_request_with_debug(
                proxy.service_name,
                method.as_str(),
                uri.path(),
                status.as_u16(),
                dur,
                "-",
                "-",
                session_id.clone(),
                cwd.clone(),
                None,
                None,
                None,
                http_debug,
            );
            return Err((status, err_str));
        }
    };
    let original_effort = extract_reasoning_effort_from_request_body(&raw_body);
    let override_effort = if let Some(id) = session_id.as_deref() {
        proxy.state.get_session_effort_override(id).await
    } else {
        None
    };
    let effective_effort = override_effort.clone().or(original_effort.clone());

    let body_for_upstream = if let Some(ref effort) = override_effort {
        Bytes::from(
            apply_reasoning_effort_override(&raw_body, effort)
                .unwrap_or_else(|| raw_body.as_ref().to_vec()),
        )
    } else {
        raw_body.clone()
    };
    let request_model = extract_model_from_request_body(body_for_upstream.as_ref());

    let filtered_body = proxy.filter.apply_bytes(body_for_upstream);
    let request_body_len = raw_body.len();
    let upstream_request_body_len = filtered_body.len();

    let debug_opt = http_debug_options();
    let warn_opt = http_warn_options();
    let debug_max = if debug_opt.enabled {
        debug_opt.max_body_bytes
    } else {
        0
    };
    let warn_max = if warn_opt.enabled {
        warn_opt.max_body_bytes
    } else {
        0
    };
    let request_body_previews = should_log_request_body_preview();
    let client_body_debug = if request_body_previews && debug_max > 0 {
        Some(make_body_preview(&raw_body, client_content_type, debug_max))
    } else {
        None
    };
    let upstream_request_body_debug = if request_body_previews && debug_max > 0 {
        Some(make_body_preview(
            &filtered_body,
            client_content_type,
            debug_max,
        ))
    } else {
        None
    };
    let client_body_warn = if request_body_previews && warn_max > 0 {
        Some(make_body_preview(&raw_body, client_content_type, warn_max))
    } else {
        None
    };
    let upstream_request_body_warn = if request_body_previews && warn_max > 0 {
        Some(make_body_preview(
            &filtered_body,
            client_content_type,
            warn_max,
        ))
    } else {
        None
    };

    let request_id = proxy
        .state
        .begin_request(
            proxy.service_name,
            method.as_str(),
            uri.path(),
            session_id.clone(),
            cwd.clone(),
            request_model.clone(),
            effective_effort.clone(),
            started_at_ms,
        )
        .await;

    let retry_opt = retry_options(&proxy.config.retry);
    let mut avoid: HashSet<usize> = HashSet::new();
    let mut upstream_chain: Vec<String> = Vec::new();

    for attempt_index in 0..retry_opt.max_attempts {
        let upstream_total = lb.service.upstreams.len();
        if upstream_total > 0 && avoid.len() >= upstream_total {
            upstream_chain.push(format!("all_upstreams_avoided total={upstream_total}"));
            break;
        }

        let Some(selected) = lb.select_upstream_avoiding(&avoid) else {
            let dur = start.elapsed().as_millis() as u64;
            let status = StatusCode::BAD_GATEWAY;
            log_request_with_debug(
                proxy.service_name,
                method.as_str(),
                uri.path(),
                status.as_u16(),
                dur,
                "-",
                "-",
                session_id.clone(),
                cwd.clone(),
                effective_effort.clone(),
                None,
                retry_info_for_chain(&upstream_chain),
                None,
            );
            proxy
                .state
                .finish_request(request_id, status.as_u16(), dur, started_at_ms + dur, None)
                .await;
            return Err((status, "no upstreams in current config".to_string()));
        };

        let target_url = match proxy.build_target(&selected, &uri) {
            Ok((url, _headers)) => url,
            Err(e) => {
                lb.record_result(selected.index, false);
                let err_str = e.to_string();
                upstream_chain.push(format!(
                    "{} (idx={}) target_build_error={}",
                    selected.upstream.base_url, selected.index, err_str
                ));
                avoid.insert(selected.index);

                let can_retry = attempt_index + 1 < retry_opt.max_attempts;
                if can_retry {
                    backoff_sleep(&retry_opt, attempt_index).await;
                    continue;
                }

                let dur = start.elapsed().as_millis() as u64;
                let status = StatusCode::BAD_GATEWAY;
                let client_headers_entries = client_headers_entries_cache
                    .get_or_init(|| header_map_to_entries(&client_headers))
                    .clone();
                let http_debug = if should_include_http_warn(status.as_u16()) {
                    Some(HttpDebugLog {
                        request_body_len: Some(request_body_len),
                        upstream_request_body_len: Some(upstream_request_body_len),
                        upstream_headers_ms: None,
                        upstream_first_chunk_ms: None,
                        upstream_body_read_ms: None,
                        upstream_error_class: Some("target_build_error".to_string()),
                        upstream_error_hint: Some(
                            "构造上游 target_url 失败（通常是 base_url 配置错误）。".to_string(),
                        ),
                        upstream_cf_ray: None,
                        client_uri: uri.to_string(),
                        target_url: "-".to_string(),
                        client_headers: client_headers_entries,
                        upstream_request_headers: Vec::new(),
                        auth_resolution: None,
                        client_body: client_body_warn.clone(),
                        upstream_request_body: upstream_request_body_warn.clone(),
                        upstream_response_headers: None,
                        upstream_response_body: None,
                        upstream_error: Some(err_str.clone()),
                    })
                } else {
                    None
                };
                log_request_with_debug(
                    proxy.service_name,
                    method.as_str(),
                    uri.path(),
                    status.as_u16(),
                    dur,
                    &selected.config_name,
                    &selected.upstream.base_url,
                    session_id.clone(),
                    cwd.clone(),
                    effective_effort.clone(),
                    None,
                    retry_info_for_chain(&upstream_chain),
                    http_debug,
                );
                proxy
                    .state
                    .finish_request(request_id, status.as_u16(), dur, started_at_ms + dur, None)
                    .await;
                return Err((status, err_str));
            }
        };

        // copy headers, stripping host/content-length and hop-by-hop.
        // auth headers:
        // - if upstream config provides a token/key, override client values;
        // - otherwise, preserve client Authorization / X-API-Key (required for requires_openai_auth=true providers).
        let mut headers = filter_request_headers(&client_headers);
        let client_has_auth = headers.contains_key("authorization");
        let (token, token_src) = resolve_auth_token_with_source(
            proxy.service_name,
            &selected.upstream.auth,
            client_has_auth,
        );
        if let Some(token) = token
            && let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}"))
        {
            headers.insert(HeaderName::from_static("authorization"), v);
        }

        let client_has_x_api_key = headers.contains_key("x-api-key");
        let (api_key, api_key_src) = resolve_api_key_with_source(
            proxy.service_name,
            &selected.upstream.auth,
            client_has_x_api_key,
        );
        if let Some(key) = api_key
            && let Ok(v) = HeaderValue::from_str(&key)
        {
            headers.insert(HeaderName::from_static("x-api-key"), v);
        }

        let upstream_request_headers = headers.clone();
        let provider_id = selected.upstream.tags.get("provider_id").cloned();
        proxy
            .state
            .update_request_route(
                request_id,
                selected.config_name.clone(),
                provider_id.clone(),
                selected.upstream.base_url.clone(),
            )
            .await;
        let auth_resolution = AuthResolutionLog {
            authorization: Some(token_src),
            x_api_key: Some(api_key_src),
        };

        let debug_base = if debug_max > 0 || warn_max > 0 {
            Some(HttpDebugBase {
                debug_max_body_bytes: debug_max,
                warn_max_body_bytes: warn_max,
                request_body_len,
                upstream_request_body_len,
                client_uri: uri.to_string(),
                target_url: target_url.to_string(),
                client_headers: client_headers_entries_cache
                    .get_or_init(|| header_map_to_entries(&client_headers))
                    .clone(),
                upstream_request_headers: header_map_to_entries(&upstream_request_headers),
                auth_resolution: Some(auth_resolution),
                client_body_debug: client_body_debug.clone(),
                upstream_request_body_debug: upstream_request_body_debug.clone(),
                client_body_warn: client_body_warn.clone(),
                upstream_request_body_warn: upstream_request_body_warn.clone(),
            })
        } else {
            None
        };

        // 详细转发日志仅在 debug 级别输出，避免刷屏。
        tracing::debug!(
            "forwarding {} {} to {} ({})",
            method,
            uri.path(),
            target_url,
            selected.config_name
        );

        let builder = proxy
            .client
            .request(method.clone(), target_url.clone())
            .headers(headers)
            .body(filtered_body.clone());

        let upstream_start = Instant::now();
        let resp = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                lb.record_result(selected.index, false);
                let err_str = e.to_string();
                upstream_chain.push(format!(
                    "{} (idx={}) transport_error={}",
                    selected.upstream.base_url, selected.index, err_str
                ));
                let can_retry = attempt_index + 1 < retry_opt.max_attempts
                    && should_retry_class(&retry_opt, Some("upstream_transport_error"));
                if can_retry {
                    lb.penalize(
                        selected.index,
                        retry_opt.transport_cooldown_secs,
                        "upstream_transport_error",
                    );
                    avoid.insert(selected.index);
                    backoff_sleep(&retry_opt, attempt_index).await;
                    continue;
                }

                let dur = start.elapsed().as_millis() as u64;
                let status_code = StatusCode::BAD_GATEWAY.as_u16();
                let upstream_headers_ms = upstream_start.elapsed().as_millis() as u64;
                let retry = retry_info_for_chain(&upstream_chain);
                let http_debug_warn = debug_base.as_ref().and_then(|b| {
                    if b.warn_max_body_bytes == 0 {
                        return None;
                    }
                    Some(HttpDebugLog {
                        request_body_len: Some(b.request_body_len),
                        upstream_request_body_len: Some(b.upstream_request_body_len),
                        upstream_headers_ms: Some(upstream_headers_ms),
                        upstream_first_chunk_ms: None,
                        upstream_body_read_ms: None,
                        upstream_error_class: Some("upstream_transport_error".to_string()),
                        upstream_error_hint: Some(
                            "上游连接/发送请求失败（reqwest 错误）；请检查网络、DNS、TLS、代理设置或上游可用性。".to_string(),
                        ),
                        upstream_cf_ray: None,
                        client_uri: b.client_uri.clone(),
                        target_url: b.target_url.clone(),
                        client_headers: b.client_headers.clone(),
                        upstream_request_headers: b.upstream_request_headers.clone(),
                        auth_resolution: b.auth_resolution.clone(),
                        client_body: b.client_body_warn.clone(),
                        upstream_request_body: b.upstream_request_body_warn.clone(),
                        upstream_response_headers: None,
                        upstream_response_body: None,
                        upstream_error: Some(err_str.clone()),
                    })
                });
                if should_include_http_warn(status_code)
                    && let Some(h) = http_debug_warn.as_ref()
                {
                    warn_http_debug(status_code, h);
                }
                let http_debug = if should_include_http_debug(status_code) {
                    debug_base.as_ref().and_then(|b| {
                        if b.debug_max_body_bytes == 0 {
                            return None;
                        }
                        Some(HttpDebugLog {
                            request_body_len: Some(b.request_body_len),
                            upstream_request_body_len: Some(b.upstream_request_body_len),
                            upstream_headers_ms: Some(upstream_headers_ms),
                            upstream_first_chunk_ms: None,
                            upstream_body_read_ms: None,
                            upstream_error_class: Some("upstream_transport_error".to_string()),
                            upstream_error_hint: Some(
                                "上游连接/发送请求失败（reqwest 错误）；请检查网络、DNS、TLS、代理设置或上游可用性。".to_string(),
                            ),
                            upstream_cf_ray: None,
                            client_uri: b.client_uri.clone(),
                            target_url: b.target_url.clone(),
                            client_headers: b.client_headers.clone(),
                            upstream_request_headers: b.upstream_request_headers.clone(),
                            auth_resolution: b.auth_resolution.clone(),
                            client_body: b.client_body_debug.clone(),
                            upstream_request_body: b.upstream_request_body_debug.clone(),
                            upstream_response_headers: None,
                            upstream_response_body: None,
                            upstream_error: Some(err_str.clone()),
                        })
                    })
                } else if should_include_http_warn(status_code) {
                    http_debug_warn.clone()
                } else {
                    None
                };
                log_request_with_debug(
                    proxy.service_name,
                    method.as_str(),
                    uri.path(),
                    status_code,
                    dur,
                    &selected.config_name,
                    &selected.upstream.base_url,
                    session_id.clone(),
                    cwd.clone(),
                    effective_effort.clone(),
                    None,
                    retry,
                    http_debug,
                );
                proxy
                    .state
                    .finish_request(request_id, status_code, dur, started_at_ms + dur, None)
                    .await;
                return Err((StatusCode::BAD_GATEWAY, e.to_string()));
            }
        };

        let upstream_headers_ms = upstream_start.elapsed().as_millis() as u64;
        let status = resp.status();
        let success = status.is_success();
        let resp_headers = resp.headers().clone();
        let resp_headers_filtered = filter_response_headers(&resp_headers);

        // 对用户对话轮次输出更有信息量的 info 日志（仅最终返回时打印，避免重试期间刷屏）。

        if is_stream && success {
            lb.record_result(selected.index, true);
            upstream_chain.push(format!(
                "{} (idx={}) status={}",
                selected.upstream.base_url,
                selected.index,
                status.as_u16()
            ));
            let retry = retry_info_for_chain(&upstream_chain);

            if is_user_turn {
                let provider_id = selected
                    .upstream
                    .tags
                    .get("provider_id")
                    .map(|s| s.as_str())
                    .unwrap_or("-");
                info!(
                    "user turn {} {} using config '{}' upstream[{}] provider_id='{}' base_url='{}'",
                    method,
                    uri.path(),
                    selected.config_name,
                    selected.index,
                    provider_id,
                    selected.upstream.base_url
                );
            }

            #[derive(Default)]
            struct StreamUsageState {
                buffer: Vec<u8>,
                logged: bool,
                finished: bool,
                warned_non_success: bool,
                first_chunk_ms: Option<u64>,
                usage: Option<crate::usage::UsageMetrics>,
                usage_scan_pos: usize,
            }

            struct StreamFinalize {
                service_name: String,
                method: String,
                path: String,
                status_code: u16,
                start: Instant,
                started_at_ms: u64,
                upstream_start: Instant,
                upstream_headers_ms: u64,
                request_body_len: usize,
                upstream_request_body_len: usize,
                config_name: String,
                upstream_base_url: String,
                retry: Option<RetryInfo>,
                session_id: Option<String>,
                cwd: Option<String>,
                reasoning_effort: Option<String>,
                request_id: u64,
                state: Arc<ProxyState>,
                resp_headers: HeaderMap,
                debug_base: Option<HttpDebugBase>,
                usage_state: Arc<Mutex<StreamUsageState>>,
            }

            impl StreamFinalize {
                fn build_http_debug(
                    &self,
                    body: &[u8],
                    first_chunk_ms: Option<u64>,
                    for_warn: bool,
                ) -> Option<HttpDebugLog> {
                    let b = self.debug_base.as_ref()?;
                    let max = if for_warn {
                        b.warn_max_body_bytes
                    } else {
                        b.debug_max_body_bytes
                    };
                    if max == 0 {
                        return None;
                    }
                    let resp_ct = self
                        .resp_headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok());
                    let (client_body, upstream_request_body) = if for_warn {
                        (
                            b.client_body_warn.clone(),
                            b.upstream_request_body_warn.clone(),
                        )
                    } else {
                        (
                            b.client_body_debug.clone(),
                            b.upstream_request_body_debug.clone(),
                        )
                    };
                    let (cls, hint, cf_ray) =
                        classify_upstream_response(self.status_code, &self.resp_headers, body);
                    Some(HttpDebugLog {
                        request_body_len: Some(self.request_body_len),
                        upstream_request_body_len: Some(self.upstream_request_body_len),
                        upstream_headers_ms: Some(self.upstream_headers_ms),
                        upstream_first_chunk_ms: first_chunk_ms,
                        upstream_body_read_ms: None,
                        upstream_error_class: cls,
                        upstream_error_hint: hint,
                        upstream_cf_ray: cf_ray,
                        client_uri: b.client_uri.clone(),
                        target_url: b.target_url.clone(),
                        client_headers: b.client_headers.clone(),
                        upstream_request_headers: b.upstream_request_headers.clone(),
                        auth_resolution: b.auth_resolution.clone(),
                        client_body,
                        upstream_request_body,
                        upstream_response_headers: Some(header_map_to_entries(&self.resp_headers)),
                        upstream_response_body: Some(make_body_preview(body, resp_ct, max)),
                        upstream_error: None,
                    })
                }
            }

            impl Drop for StreamFinalize {
                fn drop(&mut self) {
                    let state = self.state.clone();
                    let request_id = self.request_id;
                    let status_code = self.status_code;
                    let started_at_ms = self.started_at_ms;

                    let mut guard = match self.usage_state.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    if guard.finished {
                        return;
                    }
                    guard.finished = true;
                    let already_logged = guard.logged;
                    let usage_for_state = guard.usage.clone();

                    let dur = self.start.elapsed().as_millis() as u64;

                    if !already_logged {
                        guard.logged = true;
                        let usage = usage_for_state.clone();
                        let http_debug_warn =
                            self.build_http_debug(&guard.buffer, guard.first_chunk_ms, true);
                        if should_include_http_warn(self.status_code)
                            && !guard.warned_non_success
                            && let Some(h) = http_debug_warn.as_ref()
                        {
                            warn_http_debug(self.status_code, h);
                            guard.warned_non_success = true;
                        }
                        let http_debug = if should_include_http_debug(self.status_code) {
                            self.build_http_debug(&guard.buffer, guard.first_chunk_ms, false)
                        } else {
                            None
                        };
                        log_request_with_debug(
                            &self.service_name,
                            &self.method,
                            &self.path,
                            self.status_code,
                            dur,
                            &self.config_name,
                            &self.upstream_base_url,
                            self.session_id.clone(),
                            self.cwd.clone(),
                            self.reasoning_effort.clone(),
                            usage,
                            self.retry.clone(),
                            http_debug,
                        );
                    }

                    drop(guard);
                    tokio::spawn(async move {
                        state
                            .finish_request(
                                request_id,
                                status_code,
                                dur,
                                started_at_ms + dur,
                                usage_for_state,
                            )
                            .await;
                    });
                }
            }

            let max_collect = 1024 * 1024usize;
            let usage_state = Arc::new(Mutex::new(StreamUsageState::default()));
            let usage_state_inner = usage_state.clone();
            let method_s = method.to_string();
            let path_s = uri.path().to_string();
            let config_name = selected.config_name.clone();
            let base_url = selected.upstream.base_url.clone();
            let service_name = proxy.service_name.to_string();
            let start_time = start;
            let status_code = status.as_u16();

            let finalize = StreamFinalize {
                service_name: service_name.clone(),
                method: method_s.clone(),
                path: path_s.clone(),
                status_code,
                start: start_time,
                started_at_ms,
                upstream_start,
                upstream_headers_ms,
                request_body_len,
                upstream_request_body_len,
                config_name: config_name.clone(),
                upstream_base_url: base_url.clone(),
                retry: retry.clone(),
                session_id: session_id.clone(),
                cwd: cwd.clone(),
                reasoning_effort: effective_effort.clone(),
                request_id,
                state: proxy.state.clone(),
                resp_headers: resp_headers.clone(),
                debug_base,
                usage_state: usage_state.clone(),
            };

            // 对于流式用户请求，也触发一次用量查询（如 packycode），用于驱动“用完自动切换”。
            if is_user_turn && is_codex_service {
                tokio::spawn({
                    let cfg = proxy.config.clone();
                    let lb_states = proxy.lb_states.clone();
                    let config_name = selected.config_name.clone();
                    let upstream_index = selected.index;
                    async move {
                        usage_providers::poll_for_codex_upstream(
                            cfg,
                            lb_states,
                            &config_name,
                            upstream_index,
                        )
                        .await;
                    }
                });
            }

            let stream = resp
            .bytes_stream()
            .inspect_ok(move |chunk| {
                // keep finalize alive until the stream ends
                let _finalize = &finalize;

                let mut guard = match usage_state_inner.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if guard.first_chunk_ms.is_none() {
                    guard.first_chunk_ms =
                        Some(_finalize.upstream_start.elapsed().as_millis() as u64);
                }
                let remaining = max_collect.saturating_sub(guard.buffer.len());
                if remaining == 0 {
                    return;
                }
                let take = remaining.min(chunk.len());
                guard.buffer.extend_from_slice(&chunk[..take]);
                if !guard.warned_non_success && !(200..300).contains(&status_code) {
                    if should_include_http_warn(status_code)
                        && let Some(h) = _finalize.build_http_debug(
                            &guard.buffer,
                            guard.first_chunk_ms,
                            true,
                        )
                    {
                        warn_http_debug(status_code, &h);
                    } else {
                        warn!(
                            "upstream returned non-2xx status {} for {} {} (config: {}); set CODEX_HELPER_HTTP_WARN=0 to disable preview logs (or CODEX_HELPER_HTTP_DEBUG=1 for full debug)",
                            status_code, method_s, path_s, config_name
                        );
                    }
                    guard.warned_non_success = true;
                }
                if guard.logged {
                    return;
                }
                {
                    let StreamUsageState {
                        buffer,
                        usage_scan_pos,
                        usage,
                        ..
                    } = &mut *guard;
                    crate::usage::scan_usage_from_sse_bytes_incremental(
                        buffer.as_slice(),
                        usage_scan_pos,
                        usage,
                    );
                }
                if let Some(usage) = guard.usage.clone() {
                    guard.logged = true;
                    let dur = start_time.elapsed().as_millis() as u64;
                    let http_debug = if should_include_http_debug(status_code) {
                        _finalize.build_http_debug(&guard.buffer, guard.first_chunk_ms, false)
                    } else {
                        None
                    };
                    log_request_with_debug(
                        &service_name,
                        &method_s,
                        &path_s,
                        status.as_u16(),
                        dur,
                        &config_name,
                        &base_url,
                        session_id.clone(),
                        cwd.clone(),
                        effective_effort.clone(),
                        Some(usage),
                        retry.clone(),
                        http_debug,
                    );
                }
            })
            .map_err(|e| e);
            let body = Body::from_stream(stream);
            let mut builder = Response::builder().status(status);
            for (name, value) in resp_headers_filtered.iter() {
                builder = builder.header(name, value);
            }
            if resp_headers_filtered.get("content-type").is_none() {
                builder = builder.header("content-type", "text/event-stream");
            }
            return Ok(builder.body(body).unwrap());
        } else {
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    lb.record_result(selected.index, false);
                    let err_str = e.to_string();
                    upstream_chain.push(format!(
                        "{} (idx={}) body_read_error={}",
                        selected.upstream.base_url, selected.index, err_str
                    ));
                    let can_retry = attempt_index + 1 < retry_opt.max_attempts
                        && should_retry_class(&retry_opt, Some("upstream_transport_error"));
                    if can_retry {
                        lb.penalize(
                            selected.index,
                            retry_opt.transport_cooldown_secs,
                            "upstream_body_read_error",
                        );
                        avoid.insert(selected.index);
                        backoff_sleep(&retry_opt, attempt_index).await;
                        continue;
                    }

                    let dur = start.elapsed().as_millis() as u64;
                    let status = StatusCode::BAD_GATEWAY;
                    let http_debug = if should_include_http_warn(status.as_u16())
                        && let Some(b) = debug_base.as_ref()
                    {
                        Some(HttpDebugLog {
                            request_body_len: Some(b.request_body_len),
                            upstream_request_body_len: Some(b.upstream_request_body_len),
                            upstream_headers_ms: Some(upstream_headers_ms),
                            upstream_first_chunk_ms: None,
                            upstream_body_read_ms: None,
                            upstream_error_class: Some("upstream_transport_error".to_string()),
                            upstream_error_hint: Some(
                                "读取上游响应 body 失败（连接中断/解码错误等）；可视为传输错误。"
                                    .to_string(),
                            ),
                            upstream_cf_ray: None,
                            client_uri: b.client_uri.clone(),
                            target_url: b.target_url.clone(),
                            client_headers: b.client_headers.clone(),
                            upstream_request_headers: b.upstream_request_headers.clone(),
                            auth_resolution: b.auth_resolution.clone(),
                            client_body: b.client_body_warn.clone(),
                            upstream_request_body: b.upstream_request_body_warn.clone(),
                            upstream_response_headers: Some(header_map_to_entries(&resp_headers)),
                            upstream_response_body: None,
                            upstream_error: Some(err_str.clone()),
                        })
                    } else {
                        None
                    };
                    log_request_with_debug(
                        proxy.service_name,
                        method.as_str(),
                        uri.path(),
                        status.as_u16(),
                        dur,
                        &selected.config_name,
                        &selected.upstream.base_url,
                        session_id.clone(),
                        cwd.clone(),
                        effective_effort.clone(),
                        None,
                        retry_info_for_chain(&upstream_chain),
                        http_debug,
                    );
                    proxy
                        .state
                        .finish_request(request_id, status.as_u16(), dur, started_at_ms + dur, None)
                        .await;
                    return Err((status, err_str));
                }
            };
            let upstream_body_read_ms = upstream_start.elapsed().as_millis() as u64;
            let dur = start.elapsed().as_millis() as u64;
            let usage = extract_usage_from_bytes(&bytes);
            let status_code = status.as_u16();
            let (cls, hint, cf_ray) =
                classify_upstream_response(status_code, &resp_headers, bytes.as_ref());

            upstream_chain.push(format!(
                "{} (idx={}) status={} class={}",
                selected.upstream.base_url,
                selected.index,
                status_code,
                cls.as_deref().unwrap_or("-")
            ));

            let retryable = !status.is_success()
                && attempt_index + 1 < retry_opt.max_attempts
                && (should_retry_status(&retry_opt, status_code)
                    || should_retry_class(&retry_opt, cls.as_deref()));
            if retryable {
                // Treat retryable 5xx / WAF-like responses as upstream failures for LB tracking.
                if status_code >= 500 || cls.is_some() {
                    lb.record_result(selected.index, false);
                }
                let cls_s = cls.as_deref().unwrap_or("-");
                info!(
                    "retrying after non-2xx status {} (class={}) for {} {} (config: {}, next_attempt={}/{})",
                    status_code,
                    cls_s,
                    method,
                    uri.path(),
                    selected.config_name,
                    attempt_index + 2,
                    retry_opt.max_attempts
                );
                match cls.as_deref() {
                    Some("cloudflare_challenge") => lb.penalize(
                        selected.index,
                        retry_opt.cloudflare_challenge_cooldown_secs,
                        "cloudflare_challenge",
                    ),
                    Some("cloudflare_timeout") => lb.penalize(
                        selected.index,
                        retry_opt.cloudflare_timeout_cooldown_secs,
                        "cloudflare_timeout",
                    ),
                    _ => {}
                }
                avoid.insert(selected.index);
                retry_sleep(&retry_opt, attempt_index, &resp_headers).await;
                continue;
            }

            // Update LB state (final attempt):
            // - 2xx => success
            // - transport / 5xx / classified WAF failures => failure
            // - generic 3xx/4xx => neutral (do not mark upstream good/bad to avoid sticky routing to a failing upstream,
            //   and also avoid penalizing upstreams for client-side mistakes).
            if success {
                lb.record_result(selected.index, true);
            } else if status_code >= 500 || cls.is_some() {
                lb.record_result(selected.index, false);
            }

            let retry = retry_info_for_chain(&upstream_chain);

            if is_user_turn {
                let provider_id = selected
                    .upstream
                    .tags
                    .get("provider_id")
                    .map(|s| s.as_str())
                    .unwrap_or("-");
                info!(
                    "user turn {} {} using config '{}' upstream[{}] provider_id='{}' base_url='{}'",
                    method,
                    uri.path(),
                    selected.config_name,
                    selected.index,
                    provider_id,
                    selected.upstream.base_url
                );
            }

            let http_debug_warn = if should_include_http_warn(status_code)
                && let Some(b) = debug_base.as_ref()
            {
                let max = b.warn_max_body_bytes;
                let resp_ct = resp_headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok());
                Some(HttpDebugLog {
                    request_body_len: Some(b.request_body_len),
                    upstream_request_body_len: Some(b.upstream_request_body_len),
                    upstream_headers_ms: Some(upstream_headers_ms),
                    upstream_first_chunk_ms: None,
                    upstream_body_read_ms: Some(upstream_body_read_ms),
                    upstream_error_class: cls.clone(),
                    upstream_error_hint: hint.clone(),
                    upstream_cf_ray: cf_ray.clone(),
                    client_uri: b.client_uri.clone(),
                    target_url: b.target_url.clone(),
                    client_headers: b.client_headers.clone(),
                    upstream_request_headers: b.upstream_request_headers.clone(),
                    auth_resolution: b.auth_resolution.clone(),
                    client_body: b.client_body_warn.clone(),
                    upstream_request_body: b.upstream_request_body_warn.clone(),
                    upstream_response_headers: Some(header_map_to_entries(&resp_headers)),
                    upstream_response_body: Some(make_body_preview(bytes.as_ref(), resp_ct, max)),
                    upstream_error: None,
                })
            } else {
                None
            };

            if !status.is_success() {
                if let Some(h) = http_debug_warn.as_ref() {
                    warn_http_debug(status_code, h);
                } else {
                    let cls_s = cls.as_deref().unwrap_or("-");
                    let cf_ray_s = cf_ray.as_deref().unwrap_or("-");
                    warn!(
                        "upstream returned non-2xx status {} (class={}, cf_ray={}) for {} {} (config: {}); set CODEX_HELPER_HTTP_WARN=0 to disable preview logs (or CODEX_HELPER_HTTP_DEBUG=1 for full debug)",
                        status_code,
                        cls_s,
                        cf_ray_s,
                        method,
                        uri.path(),
                        selected.config_name
                    );
                }
            }

            let http_debug = if should_include_http_debug(status_code) {
                debug_base.map(|b| {
                    let max = b.debug_max_body_bytes;
                    let resp_ct = resp_headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok());
                    HttpDebugLog {
                        request_body_len: Some(b.request_body_len),
                        upstream_request_body_len: Some(b.upstream_request_body_len),
                        upstream_headers_ms: Some(upstream_headers_ms),
                        upstream_first_chunk_ms: None,
                        upstream_body_read_ms: Some(upstream_body_read_ms),
                        upstream_error_class: cls,
                        upstream_error_hint: hint,
                        upstream_cf_ray: cf_ray,
                        client_uri: b.client_uri,
                        target_url: b.target_url,
                        client_headers: b.client_headers,
                        upstream_request_headers: b.upstream_request_headers,
                        auth_resolution: b.auth_resolution,
                        client_body: b.client_body_debug,
                        upstream_request_body: b.upstream_request_body_debug,
                        upstream_response_headers: Some(header_map_to_entries(&resp_headers)),
                        upstream_response_body: Some(make_body_preview(
                            bytes.as_ref(),
                            resp_ct,
                            max,
                        )),
                        upstream_error: None,
                    }
                })
            } else if should_include_http_warn(status_code) {
                http_debug_warn.clone()
            } else {
                None
            };

            log_request_with_debug(
                proxy.service_name,
                method.as_str(),
                uri.path(),
                status_code,
                dur,
                &selected.config_name,
                &selected.upstream.base_url,
                session_id.clone(),
                cwd.clone(),
                effective_effort.clone(),
                usage.clone(),
                retry,
                http_debug,
            );
            proxy
                .state
                .finish_request(
                    request_id,
                    status_code,
                    dur,
                    started_at_ms + dur,
                    usage.clone(),
                )
                .await;

            // Poll usage once after a user request finishes (e.g. packycode), used to drive auto-switching.
            if is_user_turn && is_codex_service {
                usage_providers::poll_for_codex_upstream(
                    proxy.config.clone(),
                    proxy.lb_states.clone(),
                    &selected.config_name,
                    selected.index,
                )
                .await;
            }

            let mut builder = Response::builder().status(status);
            for (name, value) in resp_headers_filtered.iter() {
                builder = builder.header(name, value);
            }
            return Ok(builder.body(Body::from(bytes)).unwrap());
        }
    }

    let dur = start.elapsed().as_millis() as u64;
    let status = StatusCode::BAD_GATEWAY;
    let http_debug = if should_include_http_warn(status.as_u16()) {
        let client_headers_entries = client_headers_entries_cache
            .get_or_init(|| header_map_to_entries(&client_headers))
            .clone();
        Some(HttpDebugLog {
            request_body_len: Some(request_body_len),
            upstream_request_body_len: Some(upstream_request_body_len),
            upstream_headers_ms: None,
            upstream_first_chunk_ms: None,
            upstream_body_read_ms: None,
            upstream_error_class: Some("retry_exhausted".to_string()),
            upstream_error_hint: Some("所有重试尝试均未能返回可用响应。".to_string()),
            upstream_cf_ray: None,
            client_uri: uri.to_string(),
            target_url: "-".to_string(),
            client_headers: client_headers_entries,
            upstream_request_headers: Vec::new(),
            auth_resolution: None,
            client_body: client_body_warn.clone(),
            upstream_request_body: upstream_request_body_warn.clone(),
            upstream_response_headers: None,
            upstream_response_body: None,
            upstream_error: Some(format!(
                "retry attempts exhausted; chain={:?}",
                upstream_chain
            )),
        })
    } else {
        None
    };
    log_request_with_debug(
        proxy.service_name,
        method.as_str(),
        uri.path(),
        status.as_u16(),
        dur,
        "-",
        "-",
        session_id.clone(),
        cwd.clone(),
        effective_effort.clone(),
        None,
        retry_info_for_chain(&upstream_chain),
        http_debug,
    );
    proxy
        .state
        .finish_request(request_id, status.as_u16(), dur, started_at_ms + dur, None)
        .await;
    Err((status, "retry attempts exhausted".to_string()))
}

pub fn router(proxy: ProxyService) -> Router {
    // In axum 0.8, wildcard segments use `/{*path}` (equivalent to `/*path` from axum 0.7).
    #[derive(serde::Deserialize)]
    struct SessionOverrideRequest {
        session_id: String,
        effort: Option<String>,
    }

    async fn set_session_override(
        proxy: ProxyService,
        Json(payload): Json<SessionOverrideRequest>,
    ) -> Result<StatusCode, (StatusCode, String)> {
        if payload.session_id.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "session_id is required".to_string(),
            ));
        }
        if let Some(effort) = payload.effort {
            if effort.trim().is_empty() {
                return Err((StatusCode::BAD_REQUEST, "effort is empty".to_string()));
            }
            proxy
                .state
                .set_session_effort_override(
                    payload.session_id,
                    effort,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                )
                .await;
        } else {
            proxy
                .state
                .clear_session_effort_override(payload.session_id.as_str())
                .await;
        }
        Ok(StatusCode::NO_CONTENT)
    }

    async fn list_session_overrides(
        proxy: ProxyService,
    ) -> Result<Json<std::collections::HashMap<String, String>>, (StatusCode, String)> {
        let map = proxy.state.list_session_effort_overrides().await;
        Ok(Json(map))
    }

    async fn list_active_requests(
        proxy: ProxyService,
    ) -> Result<Json<Vec<ActiveRequest>>, (StatusCode, String)> {
        let vec = proxy.state.list_active_requests().await;
        Ok(Json(vec))
    }

    #[derive(serde::Deserialize)]
    struct RecentQuery {
        limit: Option<usize>,
    }

    async fn list_recent_finished(
        proxy: ProxyService,
        Query(q): Query<RecentQuery>,
    ) -> Result<Json<Vec<FinishedRequest>>, (StatusCode, String)> {
        let limit = q.limit.unwrap_or(50).clamp(1, 200);
        let vec = proxy.state.list_recent_finished(limit).await;
        Ok(Json(vec))
    }

    let p0 = proxy.clone();
    let p1 = proxy.clone();
    let p2 = proxy.clone();
    let p3 = proxy.clone();
    let p4 = proxy.clone();

    Router::new()
        .route(
            "/__codex_helper/override/session",
            get(move || list_session_overrides(p0.clone()))
                .post(move |payload| set_session_override(p1.clone(), payload)),
        )
        .route(
            "/__codex_helper/status/active",
            get(move || list_active_requests(p3.clone())),
        )
        .route(
            "/__codex_helper/status/recent",
            get(move |q| list_recent_finished(p4.clone(), q)),
        )
        .route("/{*path}", any(move |req| handle_proxy(p2.clone(), req)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn parse_status_ranges_accepts_single_codes_and_ranges() {
        assert_eq!(
            parse_status_ranges("429,500-599"),
            vec![(429, 429), (500, 599)]
        );
    }

    #[test]
    fn retry_after_ms_parses_seconds_and_caps() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("10"));
        let opt = RetryOptions {
            max_attempts: 3,
            base_backoff_ms: 200,
            max_backoff_ms: 2_000,
            jitter_ms: 0,
            retry_status_ranges: vec![(429, 429)],
            retry_error_classes: Vec::new(),
            cloudflare_challenge_cooldown_secs: 0,
            cloudflare_timeout_cooldown_secs: 0,
            transport_cooldown_secs: 0,
        };
        assert_eq!(retry_after_ms(&headers, &opt), Some(2_000));
    }
}
