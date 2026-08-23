# 01 — Monorepo 脚手架与绿色基线

**What to build:** 开发者克隆仓库后，一条命令拉起依赖服务、一条命令构建后端、一条命令启动前端占位页。仓库具备 Cargo workspace（`jiuyue-domain` / `jiuyue-protocol` / `jiuyue-server` 空壳）与 pnpm + Vite7 + Vue3 + TS 应用骨架（含路由/Pinia/i18n/Tailwind 装配与占位首页）。依赖服务双路径：`deploy/docker-compose.yml`（PG16+Redis7，标准/CI 环境）+ `deploy/local-dev.ps1`（无 Docker 主机的本机等价栈引导与就绪检查）。质量门：`cargo clippy` 零警告、`cargo test` 绿、前端 `vue-tsc --noEmit` 绿。

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] `deploy/docker-compose.yml` 提供且语法校验通过（`docker compose config`，在具备 Docker 的环境可验证）
- [ ] `deploy/local-dev.ps1` 就绪检查通过：PG16 可连（jiuyue_dev/jiuyue_test）、Redis PING=PONG
- [ ] `cargo build && cargo test && cargo clippy -- -D warnings` 全绿（workspace 三 crate）
- [ ] `pnpm install && pnpm dev` 可访问占位首页；`pnpm typecheck` 绿
- [ ] README「快速开始」含双路径说明，照做可成功
