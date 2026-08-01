# 开源打包说明

本目录由 `repos/zen-proxy-rs` 与 `repos/free-model-client-rs` 自动打包生成，用于独立部署与开源发布。

## 已剔除内容

| 类别 | 路径/内容 |
|------|-----------|
| 内部运维文档 | `docs/v4.0/*`（含内部部署记录、handoff、生产 SHA 等） |
| 备份文件 | `src/config.rs.bak` |
| 含节点引用的测试脚本 | `test_openapi.sh` |
| 内部验收矩阵 | `tests/v45_p8_acceptance_matrix.md` |
| 构建产物 | `target/` |
| 敏感配置 | `nodes.json`、`.env`（仅保留 `.example` 模板） |

## 保留内容

- 完整 Rust 源码（`zen-proxy-rs` + `free-model-client-rs`）
- 单元测试与 e2e 测试框架
- 通用配置默认值（上游地址、模型映射等通过环境变量覆盖）

## 重新打包

在 monorepo 根目录执行：

```powershell
# Windows (PowerShell)
.\scripts\package-zen-proxy-open.ps1
```

```bash
# Linux / WSL
./scripts/package-zen-proxy-open.sh
```

## 发布前检查清单

- [ ] `.env` 未包含在包内
- [ ] `nodes.json` 未包含在包内
- [ ] `cargo build --release` 通过
- [ ] `cargo test` 通过（可选，e2e 需外部依赖）
- [ ] 无硬编码 API Key / 代理凭据
