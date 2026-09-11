# JiuYue

跨平台即时通讯：文字单聊、可靠送达、离线同步、消息体验全套（已读/正在输入/撤回/回复/转发）、密聊 E2EE、好友系统（UID 搜索）、个人资料与经验等级体系、图片/视频与表情包发送。

[![Frontend](https://img.shields.io/badge/frontend-Vite%207%20%2B%20Vue%203%20%2B%20TS-42b883?logo=vuedotjs&logoColor=white)](app/)
[![Desktop](https://img.shields.io/badge/desktop-Tauri%202-24C8DB?logo=tauri&logoColor=white)](app/src-tauri/)
[![Backend](https://img.shields.io/badge/backend-Rust%20axum%20%2B%20tokio-000000?logo=rust&logoColor=white)](crates/)
[![Database](https://img.shields.io/badge/db-PostgreSQL%2016%20%2B%20Redis-4169E1?logo=postgresql&logoColor=white)](deploy/)
[![Status](https://img.shields.io/badge/status-MVP%20active-brightgreen)](#)

> **本项目由 Lecway | 联维云 独家赞助**
>
> **Exclusively sponsored by Lecway | 联维云**

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

桌面客户端：

```powershell
cd app; pnpm tauri build --no-bundle   # 产物 app/src-tauri/target/release/jiuyue-app.exe
```

## 质量门

```bash
cargo clippy --workspace -- -D warnings && cargo test --workspace
cd app && pnpm typecheck && pnpm test
```

## 仓库结构

| 路径              | 内容                                                    |
| ----------------- | ------------------------------------------------------- |
| `crates/protocol` | 版本化 WebSocket 线协议（frame envelope + golden 测试） |
| `crates/domain`   | 纯领域逻辑（seq 分配、消息状态机、XP 曲线、称号阶梯）   |
| `crates/server`   | axum HTTP + WS 服务、PG/Redis 适配                      |
| `app/`            | Vue 3 前端 + Tauri 2 桌面壳                             |
| `migrations/`     | PostgreSQL 迁移（sqlx）                                 |
| `docs/`           | 规格 / 任务票 / 调研报告                                |
| `deploy/`         | Docker Compose 与本机开发脚本                           |

## 赞助

本项目由 **Lecway | 联维云** 独家赞助。

Exclusively sponsored by **Lecway | 联维云**.
