# JiuYue

商业级跨平台即时通讯 MVP：文字单聊、可靠送达、离线同步、可选端到端加密密聊（规划）。

- 前端：Vite 7 + Vue 3 + TypeScript + Pinia + Tailwind CSS v4
- 壳：Tauri 2（Windows / macOS / Linux，iOS / Android 规划）
- 后端：Rust（axum + tokio）+ PostgreSQL 16 + Redis
- 规格：[docs/specs/jiuyue-mvp.md](docs/specs/jiuyue-mvp.md) · 任务票：[docs/tickets/m1/](docs/tickets/m1/) · 调研：[docs/research/](docs/research/README.md)

## 快速开始

### 路径 A — Docker（标准）

```bash
docker compose -f deploy/docker-compose.yml up -d   # PG16 + Redis7
cargo build                                          # 后端构建
cd app && pnpm install && pnpm dev                   # 前端 http://localhost:5173
```

### 路径 B — 本机无 Docker

前置：PostgreSQL 16 服务运行中（超级用户密码 `jiuyue_dev`，已建库 `jiuyue_dev` / `jiuyue_test`），redis-server 运行于 6379。

```powershell
.\deploy\local-dev.ps1        # 就绪检查
cargo run -p jiuyue-server    # 后端 :8080（/healthz）
cd app; pnpm install; pnpm dev
```

## 质量门

```bash
cargo clippy --workspace -- -D warnings && cargo test --workspace
cd app && pnpm typecheck && pnpm test
```
