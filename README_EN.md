# codex-proxy (Codex CLI Local Proxy)

> A Rust-based local proxy for Codex CLI traffic. It supports multi-upstream pools, failure-based circuit breaking, usage-aware routing (auto-switch on quota exhaustion), request filtering, and usage logging. The design is inspired by [cli_proxy](https://github.com/guojinpeng/cli_proxy) and [cc-switch](https://github.com/farion1231/cc-switch), but focused on Codex CLI.

## Features

- **Seamless Codex integration**
  - One command to point Codex to the local proxy: `codex-proxy switch-on`.
  - Automatically backs up `~/.codex/config.toml`, and can be restored with `switch-off`.
  - Bootstraps initial upstream config from `~/.codex` (`model_provider` + `env_key` / `auth.json`).

- **Multi-config / upstream pools**
  - All upstream configurations live in `~/.codex-proxy/config.json`.
  - Each Codex config can have:
    - A stable ID (`name`).
    - An optional human-friendly alias.
    - One or more upstreams (a “pool”).
  - CLI commands for management:
    - `config list`, `config add`, `config set-active`.

- **Load balancing & failure circuit breaking**
  - Uses weighted random selection across upstreams.
  - Each upstream tracks:
    - Consecutive failure count.
    - A cooldown deadline when failure count reaches a threshold.
  - If an upstream fails N times in a row (default 3), it enters a cooldown (default 30 seconds) and is excluded from selection during that period.

- **Usage-aware routing (“auto switch when quota is exhausted”)**
  - Introduces **usage providers**, a generic layer to query provider quotas and mark upstreams as “exhausted”.
  - Currently ships with a default provider for **packycode** (only via configuration; no packy-specific logic in LB):
    - `~/.codex-proxy/usage_providers.json` is auto-generated with a `packycode` entry on first run.
    - Upstreams whose `base_url` host matches `packycode.com` are associated with this provider.
    - The provider calls Packy’s budget API and decides whether the monthly quota is exhausted.
    - It uses the **same token** Codex uses for that upstream (or an optional env override).
  - LB behavior:
    - Normal path: prefer upstreams that are **not exhausted** and **not in cooldown**, using weights.
    - If all upstreams are marked exhausted: fallback mode ignores “exhausted” state and only respects failure/cooldown, so there is always a last-resort upstream.

- **Request filtering (redacting sensitive data)**
  - Reads rules from `~/.codex-proxy/filter.json`:

    ```jsonc
    [
      { "op": "replace", "source": "your-company.com", "target": "[REDACTED_DOMAIN]" },
      { "op": "remove",  "source": "super-secret-token" }
    ]
    ```

  - Supports a single object or an array of rules.
  - Applies filters on the request body **before** sending it upstream.

- **Usage extraction and request logging**
  - For non-streaming HTTP responses that look like Codex model responses:
    - Attempts to extract `input/output/reasoning/total_tokens` from `usage` or `response.usage`.
  - For SSE streaming responses:
    - Observes `data:` events in the SSE stream, parses JSON payloads, and keeps the last seen usage.
  - Writes per-request logs to `~/.codex-proxy/logs/requests.jsonl` including:
    - Timestamp, method, path, status code, duration, config name, upstream base_url, and usage (if parsed).

## Install & Run

### 1. Build the binary

From the project root:

```bash
cargo build --release
```

The resulting binary is at:

```bash
target/release/codex-proxy
```

You may want to add it to your `PATH` so you can run `codex-proxy` directly.

### 2. Point Codex at the local proxy (once)

Run:

```bash
codex-proxy switch-on
```

This will:

- Read `~/.codex/config.toml`.
- Backup to `~/.codex/config.toml.codex-proxy-backup` if not already present.
- Insert the following into `[model_providers]` and set `model_provider`:

```toml
[model_providers.codex_proxy]
name = "codex-proxy"
base_url = "http://127.0.0.1:3211"
wire_api = "responses"

model_provider = "codex_proxy"
```

You can override the port via `codex-proxy switch-on --port <PORT>`.

To restore your original Codex configuration:

```bash
codex-proxy switch-off
```

### 3. Start the proxy server

```bash
codex-proxy serve
```

Or with an explicit port:

```bash
codex-proxy serve --port 3211
```

On startup:

- The proxy listens on `127.0.0.1:<port>`.
- On first run, it bootstraps a default upstream from `~/.codex`:
  - Uses the current `model_provider`.
  - Resolves its `env_key` / `auth.json` into an upstream `auth_token`.

## Configuration & Pools

### Files

- Main config: `~/.codex-proxy/config.json`
  - Contains `codex` and (future) `claude` configurations.
- Filter rules: `~/.codex-proxy/filter.json`
- Usage providers: `~/.codex-proxy/usage_providers.json`
- Request logs: `~/.codex-proxy/logs/requests.jsonl`

### `config.json` layout (brief)

```jsonc
{
  "codex": {
    "active": "openai-main",
    "configs": {
      "openai-main": {
        "name": "openai-main",
        "alias": "Main OpenAI quota",
        "upstreams": [
          {
            "base_url": "https://api.openai.com/v1",
            "weight": 1.0,
            "auth": {
              "auth_token": "sk-...",
              "api_key": null
            },
            "tags": {
              "source": "codex-config",
              "provider_id": "openai"
            }
          }
        ]
      }
    }
  },
  "claude": {
    "active": null,
    "configs": {}
  }
}
```

- `name`: config ID (map key).
- `alias`: optional display name.
- `upstreams`: upstream pool:
  - `base_url`: upstream API base URL.
  - `weight`: selection weight.
  - `auth.auth_token`: used as `Authorization: Bearer <token>`.
  - `tags`: optional metadata (e.g., provenance).

### CLI configuration commands

List Codex configs:

```bash
codex-proxy config list
```

Example output:

```text
Codex configs:
  * openai-main [Main OpenAI quota] (1 upstreams)
    backup-proxy (2 upstreams)
```

Add a new config:

```bash
codex-proxy config add my-proxy \
  --base-url https://your-proxy.example.com/v1 \
  --auth-token sk-xxx \
  --weight 1.0 \
  --alias "Self-hosted proxy"
```

Set active config:

```bash
codex-proxy config set-active my-proxy
```

## Usage Providers

### `usage_providers.json`

Path: `~/.codex-proxy/usage_providers.json`. If it does not exist, the proxy will create a default file similar to:

```jsonc
{
  "providers": [
    {
      "id": "packycode",
      "kind": "budget_http_json",
      "domains": ["packycode.com"],
      "endpoint": "https://www.packycode.com/api/backend/users/info",
      "token_env": null,
      "poll_interval_secs": 60
    }
  ]
}
```

Fields:

- `id`: provider ID (for logging / distinction).
- `kind`: currently `budget_http_json`:
  - Expects a JSON response with budget/spent fields and determines whether the quota is exhausted.
- `domains`: any upstream whose `base_url` host matches one of these domains is associated with this provider.
- `endpoint`: usage API endpoint.
- `token_env`: optional env var name to override the token.
- `poll_interval_secs`: polling interval in seconds (default 60).

### Token resolution

For each provider:

1. If `token_env` is set and the env var is non-empty, that value is used.
2. Otherwise, it scans associated upstreams and takes the first non-empty `auth.auth_token` as its Bearer token.

This means that for the common case of a single token:

- Whatever token Codex uses to talk to that upstream (e.g., Packy) is reused for the usage API, without needing a separate token.

### Impact on LB

- For `budget_http_json`:
  - The provider reads `monthly_budget_usd` and `monthly_spent_usd` from the JSON response.
  - If `monthly_budget_usd > 0` and `monthly_spent_usd >= monthly_budget_usd`, the provider treats the quota as exhausted.
- All associated upstreams then get `usage_exhausted = true` in the LB state.
- LB behavior:
  - Normal path:
    - Excludes upstreams that are:
      - In failure cooldown, or
      - Marked as `usage_exhausted`.
  - Fallback path:
    - If all upstreams end up with weight 0 (e.g., all exhausted), LB recomputes weights ignoring `usage_exhausted`, only respecting failure/cooldown.
    - This ensures there is always an upstream to fall back to.

## Request Filtering & Logging

### Filtering: `~/.codex-proxy/filter.json`

Example:

```jsonc
[
  { "op": "replace", "source": "your-company.com", "target": "[REDACTED_DOMAIN]" },
  { "op": "remove",  "source": "super-secret-token" }
]
```

- Filters are applied to the request body before sending it upstream.
- The file is monitored via mtime; updates are picked up within about one second.

### Logging: `~/.codex-proxy/logs/requests.jsonl`

Each line is a JSON object, e.g.:

```jsonc
{
  "timestamp_ms": 1730000000000,
  "service": "codex",
  "method": "POST",
  "path": "/v1/responses",
  "status_code": 200,
  "duration_ms": 1234,
  "config_name": "openai-main",
  "upstream_base_url": "https://api.openai.com/v1",
  "usage": {
    "input_tokens": 123,
    "output_tokens": 456,
    "reasoning_tokens": 0,
    "total_tokens": 579
  }
}
```

You can use tools like `jq` to aggregate usage by config, upstream, or time window.

## Relationship to cli_proxy and cc-switch

This project is heavily inspired by, and intended to complement, the following tools:

- [cli_proxy](https://github.com/guojinpeng/cli_proxy)
  - A local multi-service proxy for Claude/Codex with a Web UI, model routing, filters, and “number pools”.
- [cc-switch](https://github.com/farion1231/cc-switch)
  - A desktop app for managing providers and live Codex/Claude configs safely (with atomic writes and rollback).

codex-proxy positions itself as:

- A **Rust-native**, CLI-first local proxy focused on traffic from the Codex CLI.
- Lightweight and headless by default (no UI), suitable for local machines and servers.
- Providing:
  - Safe integration with Codex config (`switch-on/off` + auto bootstrap).
  - Structured upstream management (`config.json` + CLI).
  - Unified LB state (failures + cooldown + usage exhaustion) with pluggable usage providers.

If you're already using `cli_proxy` or `cc-switch`, you can adopt codex-proxy as a more focused Codex-specific proxy layer, while still reusing your existing knowledge and patterns from those tools. 
