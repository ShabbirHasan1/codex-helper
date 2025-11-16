# codex-proxy（Codex CLI 本地代理）

> 基于 Rust 的 Codex 本地代理，支持多上游配置、失败熔断、用量驱动的自动切换、请求过滤和用量统计。设计灵感来自 [cli_proxy](https://github.com/guojinpeng/cli_proxy) 和 [cc-switch](https://github.com/farion1231/cc-switch)，但专门面向 Codex CLI。

## 功能概览

- **Codex 无感接入**
  - 一条命令将 Codex 的 `model_provider` 切到本地代理：`codex-proxy switch-on`。
  - 自动备份 `~/.codex/config.toml`，可通过 `switch-off` 一键恢复。
  - 自动从 `~/.codex` 读取当前 `model_provider` 与 token，生成初始上游配置。

- **多配置 / 号池管理**
  - 上游配置保存在 `~/.codex-proxy/config.json`。
  - 支持为每个 Codex 配置设置别名（alias），以及多个 upstream（号池）。
  - 提供 `config list/add/set-active` 命令，方便切换不同环境/中转站。

- **负载均衡与失败熔断**
  - 按权重进行上游选择（Weighted Random）。
  - 每个 upstream 维护连续失败计数：
    - 连续失败达到阈值（默认 3 次）会进入冷却期（默认 30 秒）。
    - 冷却期内不再调度该 upstream，期满后自动恢复。

- **用量驱动“用完自动切换”（统一状态）**
  - 引入“用量提供商（usage providers）”的概念，统一管理不同供应商的额度信息。
  - 当前默认支持 **packycode**（以配置形式存在，LB 中不写死品牌名）：
    - 默认生成 `~/.codex-proxy/usage_providers.json`，包含一个 `packycode` provider。
    - 按域名匹配 upstream（如 host 包含 `packycode.com`）并归属到该 provider。
    - 使用当前 Codex 上游的 token（或可选 env）去调用 packy 的预算接口。
    - 当检测到月度额度用完时，会把对应 upstream 标记为 “用量已用尽”（`usage_exhausted = true`）。
  - 负载均衡策略：
    - 正常情况下：优先在 **未用尽** 且 **未熔断** 的 upstream 中按权重选择。
    - 若全部 upstream 都标记为用量用尽：忽略“用尽”标记，仅保留失败熔断规则再挑选——保证永远有兜底。

- **请求过滤（敏感信息脱敏）**
  - 支持从 `~/.codex-proxy/filter.json` 读取过滤规则：
    - 规则格式与思路参考 [cli_proxy](https://github.com/guojinpeng/cli_proxy)，支持：
      - `{"op": "replace", "source": "...", "target": "..."}`  
      - `{"op": "remove",  "source": "..."}`。
    - 支持数组或单对象。
  - 每次请求在发送到上游前，对 body 进行字节级过滤，避免敏感数据直接发出。

- **用量统计与请求日志**
  - 对 Codex 发出的非流式响应请求：
    - 尝试从返回 JSON 的 `usage` 或 `response.usage` 中抽取 `input/output/reasoning/total_tokens`。
  - 对流式 SSE 响应：
    - 观察 SSE 流中的 `data:` 事件，解析其中的 JSON usage 字段，记录最后一次 usage。
  - 所有请求都会写入 `~/.codex-proxy/logs/requests.jsonl`，内容包括：
    - 时间戳、方法、路径、状态码、耗时、配置名、上游 base_url、usage（如解析到）。

## 安装与运行

### 1. 构建二进制

在项目根目录执行：

```bash
cargo build --release
```

生成的可执行文件位于：

```bash
target/release/codex-proxy
```

建议将其加入 `PATH`，方便直接使用 `codex-proxy` 命令。

### 2. 一次性让 Codex 使用本地代理

执行：

```bash
codex-proxy switch-on
```

- 该命令会：
  - 读取 `~/.codex/config.toml`。
  - 备份为 `~/.codex/config.toml.codex-proxy-backup`（如尚未备份）。
  - 在 `[model_providers]` 中写入：

    ```toml
    [model_providers.codex_proxy]
    name = "codex-proxy"
    base_url = "http://127.0.0.1:3211"
    wire_api = "responses"

    model_provider = "codex_proxy"
    ```

  - 如需自定义端口，可使用 `codex-proxy switch-on --port <PORT>`。

恢复原始 Codex 配置：

```bash
codex-proxy switch-off
```

### 3. 启动 proxy 服务

```bash
codex-proxy serve
```

或指定端口：

```bash
codex-proxy serve --port 3211
```

服务启动后：

- 监听 `127.0.0.1:<port>`，接受 Codex CLI 发出的 HTTP 请求（包括流式与非流式），并按配置转发至上游。
- 首次运行时，会从 `~/.codex/config.toml` 与 `~/.codex/auth.json` 自动生成一条默认上游配置：
  - 使用当前 `model_provider`。
  - 使用其 `env_key` / `auth.json` 中的 token 作为上游 `auth_token`。

## 配置与号池管理

### 配置文件位置

- 主配置：`~/.codex-proxy/config.json`
  - 包含 `codex` 与未来的 `claude` 配置。
- 请求过滤规则：`~/.codex-proxy/filter.json`
- 用量提供商配置：`~/.codex-proxy/usage_providers.json`
- 请求日志：`~/.codex-proxy/logs/requests.jsonl`

### `config.json` 结构（简要）

```jsonc
{
  "codex": {
    "active": "openai-main",
    "configs": {
      "openai-main": {
        "name": "openai-main",
        "alias": "主 OpenAI 额度",
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

- `name`：配置 ID，作为内部标识。
- `alias`：可选别名（用于展示）。
- `upstreams`：上游池：
  - `base_url`：上游 API 地址（不含 `/v1/responses` 路径）。
  - `weight`：该 upstream 的权重。
  - `auth.auth_token`：用于 `Authorization: Bearer <token>`。
  - `tags`：可选标签，便于标记来源。

### CLI 管理命令

列出所有 Codex 配置：

```bash
codex-proxy config list
```

输出示例：

```text
Codex configs:
  * openai-main [主 OpenAI 额度] (1 upstreams)
    backup-proxy (2 upstreams)
```

新增一条配置：

```bash
codex-proxy config add my-proxy \
  --base-url https://your-proxy.example.com/v1 \
  --auth-token sk-xxx \
  --weight 1.0 \
  --alias "自建中转"
```

切换当前激活配置：

```bash
codex-proxy config set-active my-proxy
```

## 用量提供商（Usage Providers）

### 配置结构

文件：`~/.codex-proxy/usage_providers.json`，首次运行会生成默认内容，如：

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

字段说明：

- `id`：提供商 ID，仅用于日志与区分。
- `kind`：类型，目前支持 `budget_http_json`（预算类用量接口）。
- `domains`：域名列表，当 upstream 的 `base_url` 所在 host 匹配这些域名时，会归属到该 provider。
- `endpoint`：用量查询 API 地址。
- `token_env`：可选的环境变量名，如果设置则优先从该 env 读取 token。
- `poll_interval_secs`：轮询间隔（秒），默认 60。

### Token 选择策略

对于每个 provider：

1. 若配置了 `token_env` 且对应环境变量存在且非空，则使用该值。
2. 否则，从归属该 provider 的 upstream 中，取第一个非空的 `auth.auth_token` 用作 Bearer token。

这意味着对于 packy 这种“一般就是一个 token” 的场景：

- Codex 使用哪个 token 调用 packy upstream，我们就默认用同一个 token 去查 packy 的额度。

### 用量与 LB 的联动

- 当用量接口返回的 JSON 中：
  - `monthly_budget_usd > 0` 且 `monthly_spent_usd >= monthly_budget_usd` 时，认为“本月额度用尽”。
- 对于该 provider 管理的所有 upstream：
  - 在 LB 状态中设置 `usage_exhausted = true`。
- 负载均衡行为：
  - 只要还有其他未用尽且未熔断的 upstream，就不会再分流到这些“用尽”节点。
  - 如果所有 upstream 都被标记为 `usage_exhausted = true`：
    - LB 会在兜底路径中忽略“用尽”标记，仅根据失败熔断规则再选一个可用节点，保证不会彻底断流。

## 请求过滤与日志

### 请求过滤：`~/.codex-proxy/filter.json`

示例：

```jsonc
[
  {
    "op": "replace",
    "source": "your-company.com",
    "target": "[REDACTED_DOMAIN]"
  },
  {
    "op": "remove",
    "source": "super-secret-token"
  }
]
```

- 代理在将请求 body 发往上游前，会按规则进行字节级替换/删除。
- 过滤规则会缓存，并在文件修改（mtime 变化）后自动重新加载（约 1 秒内生效）。

### 请求日志：`~/.codex-proxy/logs/requests.jsonl`

每行是一个 JSON 对象，类似：

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

你可以用 `jq` 等工具按配置名 / 上游 / 时间窗口做用量分析或问题排查。

- 本项目参考并借鉴了以下开源项目的设计与经验：
  - [cli_proxy](https://github.com/guojinpeng/cli_proxy)：本地多服务代理，支持 UI、过滤、模型路由、号池等。
  - [cc-switch](https://github.com/farion1231/cc-switch)：多供应商配置与切换、Codex/Claude 配置文件的安全读写与同步。
- 我们的定位：
  - 更专注于 **Codex CLI responses wire_api 的本地代理** 和 **命令行/配置驱动的多上游管理**；
  - 默认无 UI，便于在本地环境或服务器上以轻量方式部署；
  - 在架构上预留了未来扩展 Claude Code 等服务的能力。

如果你已经熟悉 `cli_proxy` 和 `cc-switch`，可以把本项目看作一个更轻量、更 Rust 化、专门为 Codex CLI 优化的“本地代理 + 配置/用量中枢”。在使用时，你也可以同时保留/配合这些工具，根据自己的工作流选择合适的组合。 
