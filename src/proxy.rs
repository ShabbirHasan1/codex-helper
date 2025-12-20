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
use crate::logging::{
    BodyPreview, HeaderEntry, HttpDebugLog, http_debug_options, http_warn_options,
    log_request_with_debug, make_body_preview, should_include_http_debug, should_include_http_warn,
};
use crate::usage::extract_usage_from_bytes;
use crate::usage_providers;

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
    let client_headers = req.headers().clone();
    let client_content_type = client_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let (target_url, _) = proxy
        .build_target(&selected, &uri)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // copy headers, stripping host/content-length and hop-by-hop.
    // auth headers:
    // - if upstream config provides a token/key, override client values;
    // - otherwise, preserve client Authorization / X-API-Key (required for requires_openai_auth=true providers).
    let mut headers = filter_request_headers(&client_headers);
    if let Some(token) = selected.upstream.auth.resolve_auth_token()
        && let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(HeaderName::from_static("authorization"), v);
    }
    if let Some(key) = selected.upstream.auth.resolve_api_key()
        && let Ok(v) = HeaderValue::from_str(&key)
    {
        headers.insert(HeaderName::from_static("x-api-key"), v);
    }

    let upstream_request_headers = headers.clone();

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
    let debug_base = if debug_max > 0 || warn_max > 0 {
        Some(HttpDebugBase {
            debug_max_body_bytes: debug_max,
            warn_max_body_bytes: warn_max,
            request_body_len,
            upstream_request_body_len,
            client_uri: uri.to_string(),
            target_url: target_url.to_string(),
            client_headers: header_map_to_entries(&client_headers),
            upstream_request_headers: header_map_to_entries(&upstream_request_headers),
            client_body_debug: if debug_max > 0 {
                Some(make_body_preview(
                    &raw_body,
                    client_content_type.as_deref(),
                    debug_max,
                ))
            } else {
                None
            },
            upstream_request_body_debug: if debug_max > 0 {
                Some(make_body_preview(
                    &filtered_body,
                    client_content_type.as_deref(),
                    debug_max,
                ))
            } else {
                None
            },
            client_body_warn: if warn_max > 0 {
                Some(make_body_preview(
                    &raw_body,
                    client_content_type.as_deref(),
                    warn_max,
                ))
            } else {
                None
            },
            upstream_request_body_warn: if warn_max > 0 {
                Some(make_body_preview(
                    &filtered_body,
                    client_content_type.as_deref(),
                    warn_max,
                ))
            } else {
                None
            },
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
        .body(filtered_body);

    let upstream_start = Instant::now();
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            lb.record_result(selected.index, false);
            let dur = start.elapsed().as_millis() as u64;
            let status_code = StatusCode::BAD_GATEWAY.as_u16();
            let err_str = e.to_string();
            let upstream_headers_ms = upstream_start.elapsed().as_millis() as u64;
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
                    upstream_error_hint: Some("上游连接/发送请求失败（reqwest 错误）；请检查网络、DNS、TLS、代理设置或上游可用性。".to_string()),
                    upstream_cf_ray: None,
                    client_uri: b.client_uri.clone(),
                    target_url: b.target_url.clone(),
                    client_headers: b.client_headers.clone(),
                    upstream_request_headers: b.upstream_request_headers.clone(),
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
                        upstream_error_hint: Some("上游连接/发送请求失败（reqwest 错误）；请检查网络、DNS、TLS、代理设置或上游可用性。".to_string()),
                        upstream_cf_ray: None,
                        client_uri: b.client_uri.clone(),
                        target_url: b.target_url.clone(),
                        client_headers: b.client_headers.clone(),
                        upstream_request_headers: b.upstream_request_headers.clone(),
                        client_body: b.client_body_debug.clone(),
                        upstream_request_body: b.upstream_request_body_debug.clone(),
                        upstream_response_headers: None,
                        upstream_response_body: None,
                        upstream_error: Some(err_str),
                    })
                })
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
                None,
                http_debug,
            );
            return Err((StatusCode::BAD_GATEWAY, e.to_string()));
        }
    };

    let upstream_headers_ms = upstream_start.elapsed().as_millis() as u64;
    let status = resp.status();
    let success = status.is_success();
    lb.record_result(selected.index, success);
    let resp_headers = resp.headers().clone();
    let resp_headers_filtered = filter_response_headers(&resp_headers);

    // Codex Responses API 在本地代理层的路径通常为 `/responses`，
    // 实际上游 base_url 可能已包含 `/v1` 前缀（例如 https://api.openai.com/v1），
    // 因此这里仅根据是否以 `/responses` 结尾来识别“用户轮次”请求。
    let path = uri.path();
    let is_responses_path = path.ends_with("/responses");
    let is_user_turn = method == Method::POST && is_responses_path;
    let is_codex_service = proxy.service_name == "codex";

    // 对用户对话轮次输出更有信息量的 info 日志。
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

    if is_stream {
        #[derive(Default)]
        struct StreamUsageState {
            buffer: Vec<u8>,
            logged: bool,
            warned_non_success: bool,
            first_chunk_ms: Option<u64>,
        }

        struct StreamFinalize {
            service_name: String,
            method: String,
            path: String,
            status_code: u16,
            start: Instant,
            upstream_start: Instant,
            upstream_headers_ms: u64,
            request_body_len: usize,
            upstream_request_body_len: usize,
            config_name: String,
            upstream_base_url: String,
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
                let mut guard = match self.usage_state.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if guard.logged {
                    return;
                }
                guard.logged = true;

                let dur = self.start.elapsed().as_millis() as u64;
                let usage = crate::usage::extract_usage_from_sse_bytes(&guard.buffer);
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
                    usage,
                    http_debug,
                );
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
            upstream_start,
            upstream_headers_ms,
            request_body_len,
            upstream_request_body_len,
            config_name: config_name.clone(),
            upstream_base_url: base_url.clone(),
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
                            "upstream returned non-2xx status {} for {} {} (config: {}); set CODEX_HELPER_HTTP_WARN=1 to log headers/body preview",
                            status_code, method_s, path_s, config_name
                        );
                    }
                    guard.warned_non_success = true;
                }
                if guard.logged {
                    return;
                }
                if let Some(usage) = crate::usage::extract_usage_from_sse_bytes(&guard.buffer) {
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
                        Some(usage),
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
        Ok(builder.body(body).unwrap())
    } else {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        let upstream_body_read_ms = upstream_start.elapsed().as_millis() as u64;
        let dur = start.elapsed().as_millis() as u64;
        let usage = extract_usage_from_bytes(&bytes);

        if !status.is_success() {
            let status_code = status.as_u16();
            if should_include_http_warn(status_code)
                && let Some(b) = debug_base.as_ref()
            {
                let max = b.warn_max_body_bytes;
                let resp_ct = resp_headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok());
                let (cls, hint, cf_ray) =
                    classify_upstream_response(status_code, &resp_headers, bytes.as_ref());
                let http_debug = HttpDebugLog {
                    request_body_len: Some(b.request_body_len),
                    upstream_request_body_len: Some(b.upstream_request_body_len),
                    upstream_headers_ms: Some(upstream_headers_ms),
                    upstream_first_chunk_ms: None,
                    upstream_body_read_ms: Some(upstream_body_read_ms),
                    upstream_error_class: cls,
                    upstream_error_hint: hint,
                    upstream_cf_ray: cf_ray,
                    client_uri: b.client_uri.clone(),
                    target_url: b.target_url.clone(),
                    client_headers: b.client_headers.clone(),
                    upstream_request_headers: b.upstream_request_headers.clone(),
                    client_body: b.client_body_warn.clone(),
                    upstream_request_body: b.upstream_request_body_warn.clone(),
                    upstream_response_headers: Some(header_map_to_entries(&resp_headers)),
                    upstream_response_body: Some(make_body_preview(bytes.as_ref(), resp_ct, max)),
                    upstream_error: None,
                };
                warn_http_debug(status_code, &http_debug);
            } else {
                warn!(
                    "upstream returned non-2xx status {} for {} {} (config: {}); set CODEX_HELPER_HTTP_WARN=1 to log headers/body preview",
                    status_code,
                    method,
                    uri.path(),
                    selected.config_name
                );
            }
        }

        let status_code = status.as_u16();
        let http_debug = if should_include_http_debug(status_code) {
            debug_base.map(|b| {
                let max = b.debug_max_body_bytes;
                let resp_ct = resp_headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok());
                let (cls, hint, cf_ray) =
                    classify_upstream_response(status_code, &resp_headers, bytes.as_ref());
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
                    client_body: b.client_body_debug,
                    upstream_request_body: b.upstream_request_body_debug,
                    upstream_response_headers: Some(header_map_to_entries(&resp_headers)),
                    upstream_response_body: Some(make_body_preview(bytes.as_ref(), resp_ct, max)),
                    upstream_error: None,
                }
            })
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
            usage.clone(),
            http_debug,
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

        let mut builder = Response::builder().status(status);
        for (name, value) in resp_headers_filtered.iter() {
            builder = builder.header(name, value);
        }
        Ok(builder.body(Body::from(bytes)).unwrap())
    }
}

pub fn router(proxy: ProxyService) -> Router {
    // axum 0.8 中，通配段需要使用 `/{*path}` 语法，这里保持与 0.7
    // 时代的 `/*path` 行为等价：匹配任意路径并交给代理处理。
    Router::new().route("/{*path}", any(move |req| handle_proxy(proxy.clone(), req)))
}
