# 01 — Monorepo 脚手架与绿色基线

**What to build:** 开发者克隆仓库后，一条命令拉起依赖容器、一条命令构建后端、一条命令启动前端占位页。仓库具备 Cargo workspace（`jiuyue-domain` / `jiuyue-protocol` / `jiuyue-server` 空壳）与 pnpm + Vite7 + Vue3 + TS 应用骨架（含路由/Pinia/i18n/Tailwind 装配与占位首页），以及 `deploy/docker-compose.yml`（PostgreSQL 16 + Redis 7，健康检查就绪）。提交钩子级质量门：`cargo clippy` 零警告、`cargo test` 绿、前端 `vue-tsc --noEmit` 绿。

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] `docker compose -f deploy/docker-compose.yml up -d` 后 PG/Redis 均通过健康检查
- [ ] `cargo build && cargo test && cargo clippy -- -D warnings` 全绿（workspace 三 crate）
- [ ] `pnpm install && pnpm dev` 可访问占位首页；`pnpm typecheck` 绿
- [ ] README「快速开始」三步可照做成功
