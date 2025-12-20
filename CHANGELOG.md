# Changelog
All notable changes to this project will be documented in this file.

## [0.2.0] - 2025-12-20
### Added
- Safe-by-default auth config: store secrets via env vars using `auth_token_env` / `api_key_env` (instead of writing tokens to disk).
- CLI support for env-based auth: `codex-helper config add --auth-token-env ...` / `--api-key-env ...`.
- Optional HTTP debugging logs (`http_debug`) with header/body previews, timing metrics, and Cloudflare/WAF detection hints.
- Request log controls:
  - automatic rotation and retention for `requests.jsonl` (and debug logs),
  - optional `CODEX_HELPER_REQUEST_LOG_ONLY_ERRORS=1`,
  - optional split debug log file `requests_debug.jsonl` (via `CODEX_HELPER_HTTP_DEBUG_SPLIT=1`).
- `doctor` checks for missing auth env vars and plaintext secrets in `~/.codex-helper/config.json`.

### Changed
- Codex bootstrap/import prefers recording the upstream `env_key` as `auth_token_env` (no longer persisting the token by default).
- Non-2xx terminal warnings no longer include response body previews unless explicitly enabled.

### Fixed
- Proxy auth handling for `requires_openai_auth=true` providers: preserve client `Authorization` when no upstream token is configured.
- Proxy URL construction when `base_url` includes a path prefix (avoid double-prefixing like `/v1/v1/...`).
- Hop-by-hop header filtering and safer response header forwarding for streaming/non-streaming responses.
- Request body filter fallback for invalid regex rules (avoid corrupting payloads).
- Session rollout filename UUID parsing, and deterministic `active_config()` fallback selection.

