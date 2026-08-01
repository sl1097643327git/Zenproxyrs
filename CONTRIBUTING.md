# Contributing

感谢对本项目的关注。

## 开发环境

```bash
git clone <your-fork>
cd zen-proxy-rs
cp .env.example .env
./setup.sh
cd zen-proxy-rs
cargo test
```

## 提交规范

- 不要提交 `.env`、`nodes.json` 或任何密钥
- 保持 `cargo fmt` / `cargo clippy` 通过
- PR 请附带测试说明

## 报告问题

请使用 GitHub Issues，并避免在 issue 中粘贴真实 API Key 或代理凭据。
