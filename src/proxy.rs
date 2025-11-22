use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::any;
use futures_util::TryStreamExt;
use reqwest::Client;
use tracing::{info, instrument, warn};

use crate::config::{ProxyConfig, ServiceConfigManager};
use crate::filter::RequestFilter;
use crate::lb::{LbState, LoadBalancer, SelectedUpstream};
use crate::logging::log_request;
use crate::usage::extract_usage_from_bytes;
use crate::usage_providers;

/// 通用代理服务，当前只用于 codex，未来可以按 service_name 拓展。
#[derive(Clone)]
pub struct ProxyService {
    pub client: Client,
    pub config: Arc<ProxyConfig>,
    pub service_name: &'static str,
    lb_states: Arc<Mutex<HashMap<String, LbState>>>,
    filter: RequestFilter,
}

impl ProxyService {
    pub fn new(
        client: Client,
        config: Arc<ProxyConfig>,
        service_name: &'static str,
        lb_states: Arc<Mutex<HashMap<String, LbState>>>,
    ) -> Self {
        Self {
            client,
            config,
            service_name,
            lb_states,
            filter: RequestFilter::new(),
        }
    }

    fn service_manager(&self) -> &ServiceConfigManager {
        match self.service_name {
            "codex" => &self.config.codex,
            "claude" => &self.config.claude,
            _ => &self.config.codex,
        }
    }

    fn lb(&self) -> Option<LoadBalancer> {
        let mgr = self.service_manager();
        let svc = mgr.active_config()?;
        Some(LoadBalancer::new(
            Arc::new(svc.clone()),
            self.lb_states.clone(),
        ))
    }

    fn build_target(
        &self,
        upstream: &SelectedUpstream,
        uri: &Uri,
    ) -> Result<(reqwest::Url, HeaderMap)> {
        let base = upstream.upstream.base_url.trim_end_matches('/').to_string();
        let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let full = format!("{base}{path_and_query}");
        let url =
            reqwest::Url::parse(&full).map_err(|e| anyhow!("invalid upstream url {full}: {e}"))?;

        // ensure query preserved (Url::parse already includes it)
        let headers = HeaderMap::new();
        Ok((url, headers))
    }
}

#[instrument(skip_all, fields(service = %proxy.service_name))]
pub async fn handle_proxy(
    proxy: ProxyService,
    req: Request<Body>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let start = Instant::now();
    let lb = proxy.lb().ok_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "no active upstream config".to_string(),
        )
    })?;
    let selected = lb.select_upstream().ok_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "no upstreams in current config".to_string(),
        )
    })?;

    let uri = req.uri().clone();
    let method = req.method().clone();
    let (target_url, mut headers) = proxy
        .build_target(&selected, &uri)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // copy headers, stripping auth/host/content-length; we'll set our own
    let mut auth_header: Option<HeaderValue> = None;
    let mut api_key_header: Option<HeaderValue> = None;
    for (name, value) in req.headers().iter() {
        let name_str = name.as_str().to_ascii_lowercase();
        if matches!(
            name_str.as_str(),
            "authorization" | "host" | "content-length" | "x-api-key"
        ) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    if let Some(token) = &selected.upstream.auth.auth_token {
        let value = format!("Bearer {token}");
        auth_header = HeaderValue::from_str(&value).ok();
    }
    if let Some(key) = &selected.upstream.auth.api_key {
        api_key_header = HeaderValue::from_str(key).ok();
    }

    if let Some(v) = auth_header {
        headers.insert(HeaderName::from_static("authorization"), v);
    }
    if let Some(v) = api_key_header {
        headers.insert(HeaderName::from_static("x-api-key"), v);
    }

    // 检测流式
    let is_stream = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false);

    // 读取并应用过滤器
    let body = req.into_body();
    let raw_body = to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let filtered_body = proxy.filter.apply(&raw_body);

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
        .body(filtered_body);

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            lb.record_result(selected.index, false);
            let dur = start.elapsed().as_millis() as u64;
            log_request(
                proxy.service_name,
                method.as_str(),
                uri.path(),
                StatusCode::BAD_GATEWAY.as_u16(),
                dur,
                &selected.config_name,
                &selected.upstream.base_url,
                None,
            );
            return Err((StatusCode::BAD_GATEWAY, e.to_string()));
        }
    };

    let status = resp.status();
    let success = status.is_success();
    lb.record_result(selected.index, success);

    // Codex Responses API 在本地代理层的路径通常为 `/responses`，
    // 实际上游 base_url 可能已包含 `/v1` 前缀（例如 https://api.openai.com/v1），
    // 因此这里仅根据是否以 `/responses` 结尾来识别“用户轮次”请求。
    let path = uri.path();
    let is_responses_path = path.ends_with("/responses");
    let is_user_turn = method == Method::POST && is_responses_path;
    let is_codex_service = proxy.service_name == "codex";

    // 对用户对话轮次输出更有信息量的 info 日志。
    if is_user_turn {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "<unknown>".to_string());
        // 当前实现中，SelectedUpstream 只携带 config_name，别名可以在后续重构中通过扩展结构体补充。
        info!(
            "user turn {} {} using config '{}' (cwd: {})",
            method,
            uri.path(),
            selected.config_name,
            cwd
        );
    }

    if is_stream {
        #[derive(Default)]
        struct StreamUsageState {
            buffer: Vec<u8>,
            logged: bool,
            warned_non_success: bool,
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
                let mut guard = match usage_state_inner.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let remaining = max_collect.saturating_sub(guard.buffer.len());
                if remaining == 0 {
                    return;
                }
                let take = remaining.min(chunk.len());
                guard.buffer.extend_from_slice(&chunk[..take]);
                if !guard.warned_non_success && !(200..300).contains(&status_code) {
                    let preview_len = guard.buffer.len().min(512);
                    let body_preview = String::from_utf8_lossy(&guard.buffer[..preview_len]);
                    warn!(
                        "upstream returned non-2xx status {} for {} {} (config: {}): {}",
                        status_code, method_s, path_s, config_name, body_preview
                    );
                    guard.warned_non_success = true;
                }
                if guard.logged {
                    return;
                }
                if let Some(usage) = crate::usage::extract_usage_from_sse_bytes(&guard.buffer) {
                    guard.logged = true;
                    let dur = start_time.elapsed().as_millis() as u64;
                    log_request(
                        &service_name,
                        &method_s,
                        &path_s,
                        status.as_u16(),
                        dur,
                        &config_name,
                        &base_url,
                        Some(usage),
                    );
                }
            })
            .map_err(|e| e);
        let body = Body::from_stream(stream);
        Ok(Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap())
    } else {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        let dur = start.elapsed().as_millis() as u64;
        let usage = extract_usage_from_bytes(&bytes);

        if !status.is_success() {
            let preview_len = bytes.len().min(512);
            let body_preview = String::from_utf8_lossy(&bytes[..preview_len]);
            warn!(
                "upstream returned non-2xx status {} for {} {} (config: {}): {}",
                status.as_u16(),
                method,
                uri.path(),
                selected.config_name,
                body_preview
            );
        }

        log_request(
            proxy.service_name,
            method.as_str(),
            uri.path(),
            status.as_u16(),
            dur,
            &selected.config_name,
            &selected.upstream.base_url,
            usage.clone(),
        );

        // 在用户请求完成后触发一次用量查询（如 packycode），用于驱动“用完自动切换”
        if is_user_turn && is_codex_service {
            usage_providers::poll_for_codex_upstream(
                proxy.config.clone(),
                proxy.lb_states.clone(),
                &selected.config_name,
                selected.index,
            )
            .await;
        }

        Ok(Response::builder()
            .status(status)
            .body(Body::from(bytes))
            .unwrap())
    }
}

pub fn router(proxy: ProxyService) -> Router {
    // axum 0.8 中，通配段需要使用 `/{*path}` 语法，这里保持与 0.7
    // 时代的 `/*path` 行为等价：匹配任意路径并交给代理处理。
    Router::new().route("/{*path}", any(move |req| handle_proxy(proxy.clone(), req)))
}
