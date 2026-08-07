# Zen Proxy RS

高性能 Rust 实现的 OpenAI 兼容 API 反向代理，面向 Claude Code、Hermes、OpenClaw 等客户端场景。支持代理节点池调度、协议修复、动态模型发现与可观测性管理接口。

本仓库为可独立部署的开源发行版，已移除内部运维文档、节点配置与密钥等敏感信息。

## 功能概览

- OpenAI 兼容 `/v1/*` 代理转发
- 可选 `free_model_kernel` 模式（内嵌 `free-model-client-rs` 内核）
- SOCKS5/HTTP 代理节点池与故障隔离
- **Clash/mihomo 模式**：单实例多监听端口 + 独立 Selector 组，驱动 Clash 内部节点切换（详情见下文）
- 动态模型发现与探针（可选）
- Prometheus `/metrics`、管理后台 `/admin/*`、Web 状态面板
- Redis 全局预算与会话亲和（可选）

## 快速开始

### 前置要求

- Rust 1.75+（推荐 stable）
- 可选：Redis（全局预算 / 会话 pin）

### 本地运行

```bash
cp .env.example .env
# 编辑 .env，至少设置 UPSTREAM_API_KEY

cd zen-proxy-rs
cargo build --release
set -a && source ../.env && set +a   # Linux/macOS
./target/release/zen-proxy-rs
```

默认监听 `127.0.0.1:4000`。

### Docker 部署（推荐）

#### 1. 克隆仓库

```bash
git clone <repo-url>
cd Zenproxyrs
```

#### 2. 复制并配置 `.env`

```bash
cp .env.example .env
```

Docker 的**唯一功能必填项**是 `SUBSCRIBE_URL`（你的 Clash 订阅地址），但生产环境务必同时设置 `PROXY_API_KEY`、`ADMIN_API_KEY` 与 `MIHOMO_SECRET`，否则接口无鉴权、mihomo 调试口使用默认密钥。

最小可用 `.env`：

```bash
SUBSCRIBE_URL=<subscription-url>
PROXY_API_KEY=<proxy-api-key>      # 客户端（Claude Code 等）连接用
ADMIN_API_KEY=<admin-api-key>     # 管理接口 /admin/* 用
MIHOMO_SECRET=<mihomo-secret>     # mihomo 调试口鉴权
```

#### 3. 启动

```bash
docker compose up -d --build
```

容器内自动完成：
1. 启动 mihomo，用订阅地址生成两个独立 Selector 组（默认 `Group-A` / `Group-B`）
2. 两个本地混合端口（默认 `32000`/`32001`）分别绑定一组，互不影响
3. 启动 zen-proxy-rs（clash 模式），自动发现组名并驱动节点切换

#### 4. 查看状态

```bash
docker compose ps
docker compose logs -f zen-proxy
```

#### 5. 更新

本仓库通过 `build: .` 从源码构建，无预发布镜像。更新时拉取最新代码并重建即可：

```bash
git pull
docker compose up -d --build
```

`docker compose up -d --build` 只会重建受影响的 `zen-proxy` 服务，`dashboard` 与 `.env` 保持不变。

### 端口与安全

| 容器 | 宿主端口（默认） | 容器端口 | 说明 |
|------|------------------|----------|------|
| API | `${PORT:-31000}` | `4000` | OpenAI 兼容接口，Claude Code 接这里 |
| 状态面板 | `${DASH_PORT:-31001}` | `80` | Web 仪表盘（浏览器打开） |
| mihomo 出口 1 | `${MIHOMO_PORT_1:-32000}` | `32000` | 绑定 `Group-A` |
| mihomo 出口 2 | `${MIHOMO_PORT_2:-32001}` | `32001` | 绑定 `Group-B` |
| mihomo API | `${MIHOMO_API_PORT:-33000}` | `33000` | 调试口，需 `MIHOMO_SECRET` |

> 服务器部署：确保云安全组放行 `31000/31001/32000/32001/33000`。若仅需 Claude Code 与面板，可只放行 `31000/31001`。

## 目录结构

```text
.
├── free-model-client-rs/   # 内嵌上游协议内核库
├── zen-proxy-rs/           # 主代理服务
├── .env.example            # 环境变量模板
├── nodes.json.example      # 代理节点池示例
├── Dockerfile
├── docker-compose.yml
└── setup.sh                # 一键构建脚本
```

## 核心环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `PORT` | `4000` | 监听端口 |
| `BIND_ADDRESS` | `127.0.0.1` | 绑定地址 |
| `UPSTREAM_BASE` | `https://opencode.ai/zen` | 上游 API 根地址 |
| `UPSTREAM_API_KEY` | `public` | 上游 API Key |
| `PROXY_API_KEY` | _(空)_ | 客户端鉴权 Key，留空则不校验 |
| `ADMIN_API_KEY` | _(空)_ | 管理接口鉴权 Key |
| `ZEN_PROVIDER_MODE` | `legacy` | `legacy` 或 `free_model_kernel` |
| `NODES_FILE` | `/etc/zen-proxy/nodes.json` | 代理节点列表文件 |
| `PREFERRED_PROXY_URLS` | _(空)_ | 优先代理 URL，逗号分隔 |
| `GLOBAL_BUDGET_REDIS_URL` | _(空)_ | Redis 地址（全局预算） |
| `NODE_PROVIDER_MODE` | `webshare` | 节点来源：`webshare` 或 `clash` |
| `CLASH_API_URLS` | _(空)_ | Clash/mihomo API 地址，逗号分隔 |
| `CLASH_API_SECRETS` | _(空)_ | 对应 API 的 secret，逗号分隔（可省略） |
| `CLASH_PROXY_URLS` | _(空)_ | 各监听端口 socks5 地址，逗号分隔 |
| `CLASH_GROUP_NAMES` | _(自动)_ | 各端口绑定的策略组名；留空自动发现 |
| `CLASH_CONFIG_FILE` | _(空)_ | mihomo config.yaml 路径，用于按端口自动匹配组名 |
| `CLASH_SWITCH_MAX_ATTEMPTS` | `15` | 切换内部节点最大尝试次数 |
| `CLASH_INVALID_TTL_SECS` | `86400` | 无效节点黑名单 TTL（秒） |
| `NODE_5XX_BREAK_THRESHOLD` | `10` | 节点**连续** 5xx 次数达到该阈值即触发熔断（断断续续不累计，中间任一成功即清零） |
| `NODE_5XX_BREAK_COOLDOWN_SECS` | `60` | 熔断持续时长（秒），期间节点不再接真实请求 |
| `NODE_5XX_PROBE_INTERVAL_MS` | `1000` | 熔断期间后台恢复探测间隔（毫秒） |
| `NODE_5XX_PROBE_SUCCESSES` | `2` | 恢复探测需**连续**成功次数，达到即解除熔断 |

完整列表见 [.env.example](./.env.example)。

## 代理节点配置

将 `nodes.json.example` 复制为节点文件，例如：

```bash
sudo mkdir -p /etc/zen-proxy
sudo cp nodes.json.example /etc/zen-proxy/nodes.json
```

支持 JSON 数组或 `host:port:user:pass` 行格式。

## Clash 模式

将 `NODE_PROVIDER_MODE=clash` 后，不再读 `nodes.json`，而是把**每个 Clash/mihomo 监听端口当作一个节点**，通过 Clash API 驱动其内部节点切换。

### 工作原理

1. 每个端口（socks5 地址）对应一个 `ClashInstance`，绑定一个 Selector 策略组
2. 请求打到某端口 → 该组当前选中的内部节点 → 上游
3. 节点不可用（429/超时/5xx）→ 自动调 Clash API 切换该组到其他内部节点 → 探活 → 恢复
4. 维护两类状态避免误切：
   - **in_use**：各实例当前选中节点，切换时跳过其他实例正在用的节点（防两个端口同时挂）
   - **invalid**：探活失败的节点进黑名单（TTL 内不再选），过期自动释放

### 本地手动配置示例（.env）

```bash
NODE_PROVIDER_MODE=clash
CLASH_API_URLS=http://127.0.0.1:9090
CLASH_API_SECRETS=your_secret
CLASH_PROXY_URLS=socks5://127.0.0.1:7890,socks5://127.0.0.1:7891
CLASH_GROUP_NAMES=Group-A,Group-B        # 留空则自动发现
CLASH_CONFIG_FILE=/etc/mihomo/config.yaml # 自动按端口匹配组名（优先级高于 API 枚举）
```

四组列表按下标一一对应：第 N 个 API ↔ 第 N 个 secret ↔ 第 N 个端口 ↔ 第 N 个组名。单 API 驱动多端口时只需填一个 API（后续端口自动复用最后一个）。

### 组名自动发现

`CLASH_GROUP_NAMES` 留空时按优先级自动发现：
1. `CLASH_CONFIG_FILE` 存在 → 解析 mihomo config.yaml 的 `listeners` 段，按端口匹配组名（最准确）
2. 否则 → 调 Clash API 枚举 Selector 组，按序分配（多组场景顺序可能与实际绑定不符，建议用方案 1 或显式配置）

### Docker 使用

Docker 方案无需手动配置以上任何项——entrypoint 自动生成配置并用 `CLASH_CONFIG_FILE` 自动发现组名，只需在 `.env` 设置 `SUBSCRIBE_URL`。

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/metrics` | Prometheus 指标 |
| GET | `/v1/models` | 模型列表 |
| ANY | `/v1/*` | OpenAI 兼容代理 |
| * | `/admin/*` | 管理接口（需 `ADMIN_API_KEY`） |

## Web 状态面板

`zen-proxy-dashboard.html` 是单文件零依赖仪表盘，展示运行概览、节点池、请求统计、Clash 实例与最近请求。

- **Docker 部署**：浏览器打开 `http://<服务器>:31001`
- **本地**：直接双击打开 HTML，右上角填 API 地址与 `ADMIN_API_KEY`

页面默认 API 地址自动跟随当前访问域名（`http://<主机>:31000`），也可手动修改并保存（localStorage 记忆）。

## 管理接口速查

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/admin/runtime` | 运行状态 + Clash 快照 |
| GET | `/admin/pool/state` | 节点池各池状态与预算 |
| GET | `/admin/stats` | 请求统计（429/4xx/5xx） |
| GET | `/admin/requests/recent` | 最近请求记录 |
| GET | `/admin/clash/now` | 各 Clash 实例当前选中节点 |
| POST | `/admin/nodes/{id}/probe` | 手动探活 |
| POST | `/admin/nodes/{id}/recover` | 手动恢复节点到调度池 |
| POST | `/admin/probe/now` | 全量探活 |

全部需 `Authorization: Bearer <ADMIN_API_KEY>`。

## API 调用示例

以下示例假设服务部署在 `<server-host>`，端口为默认值。`<proxy-api-key>` 对应 `PROXY_API_KEY`，`<admin-api-key>` 对应 `ADMIN_API_KEY`。

### 健康检查

```bash
curl http://<server-host>:31000/health
```

### 模型列表

```bash
curl http://<server-host>:31000/v1/models \
  -H "Authorization: Bearer <proxy-api-key>"
```

### 对话补全（OpenAI 兼容）

```bash
curl http://<server-host>:31000/v1/chat/completions \
  -H "Authorization: Bearer <proxy-api-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "<model-name>",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### 管理接口（需 `ADMIN_API_KEY`）

```bash
# 各 Clash 实例当前选中节点
curl http://<server-host>:31000/admin/clash/now \
  -H "Authorization: Bearer <admin-api-key>"

# 运行状态 + Clash 快照
curl http://<server-host>:31000/admin/runtime \
  -H "Authorization: Bearer <admin-api-key>"
```

### 状态面板

浏览器打开 `http://<server-host>:31001`，右上角填 API 地址（`http://<server-host>:31000`）与 `ADMIN_API_KEY`。

## 构建与测试

```bash
./setup.sh
cd zen-proxy-rs
cargo test
```

## 许可证

MIT License — 见 [LICENSE](./LICENSE)。

## 安全说明

- 切勿将 `.env`、`nodes.json` 或真实 API Key 提交到版本库
- 生产环境务必设置 `PROXY_API_KEY` 与 `ADMIN_API_KEY`
- 本发行版已剔除内部部署记录、运维 handoff 文档及测试脚本中的节点引用
