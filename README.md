# Zen Proxy RS

高性能 Rust 实现的 OpenAI 兼容 API 反向代理，面向 Claude Code、Hermes、OpenClaw 等客户端场景。支持代理节点池调度、协议修复、动态模型发现与可观测性管理接口。

本仓库为可独立部署的开源发行版，已移除内部运维文档、节点配置与密钥等敏感信息。

## 功能概览

- OpenAI 兼容 `/v1/*` 代理转发
- 可选 `free_model_kernel` 模式（内嵌 `free-model-client-rs` 内核）
- SOCKS5/HTTP 代理节点池与故障隔离
- 动态模型发现与探针（可选）
- Prometheus `/metrics`、管理后台 `/admin/*`
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

### Docker

```bash
cp .env.example .env
# 编辑 .env
docker compose up -d --build
```

服务地址：`http://127.0.0.1:4000`

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

完整列表见 [.env.example](./.env.example)。

## 代理节点配置

将 `nodes.json.example` 复制为节点文件，例如：

```bash
sudo mkdir -p /etc/zen-proxy
sudo cp nodes.json.example /etc/zen-proxy/nodes.json
```

支持 JSON 数组或 `host:port:user:pass` 行格式。

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/metrics` | Prometheus 指标 |
| GET | `/v1/models` | 模型列表 |
| ANY | `/v1/*` | OpenAI 兼容代理 |
| * | `/admin/*` | 管理接口（需 `ADMIN_API_KEY`） |

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
