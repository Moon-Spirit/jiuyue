# jiuyue — 项目级工作指南

> 本文件只对本项目生效，补充全局 `~/.config/opencode/AGENTS.md`。
> 领域语言以 [`CONTEXT.md`](./CONTEXT.md) 为准，架构决策以 [`docs/adr/`](./docs/adr/) 为准。
> 最后更新：2026-09-16

---

## 项目定位

一个**产品级即时通讯应用**：单聊 / 群聊 / 音视频通话 / 端到端加密秘密聊天 / 离线推送 / 管理后台。

- **架构设计目标**：百万级注册（架构天花板）
- **首发部署**：单台 2C2G 香港云服务器（无备案负担），Docker Compose
- **目标端**：Web（桌面优先）+ PWA + Tauri v2 桌面端；移动端以 PWA 形态覆盖
- **界面语言**：中文优先，保留 i18n 基础设施

## 技术栈

| 层        | 选型                                                  |
| --------- | ----------------------------------------------------- |
| 后端      | Rust + Axum（WebSocket 实时 + REST）                  |
| 数据库    | PostgreSQL 16 + sqlx                                  |
| 缓存/状态 | Redis（presence / 限流 / 跨节点广播预留）             |
| 前端      | Vue 3 + Vite + TypeScript + Pinia 4                   |
| 样式      | Tailwind CSS v4 + Reka UI 原语 + Naive UI（管理后台） |
| 桌面      | Tauri v2                                              |
| 音视频    | 1:1 = P2P WebRTC + coturn；群组 = LiveKit（独立节点） |
| E2EE      | vodozemac（秘密聊天专用）                             |
| 部署      | Docker Compose + Caddy + GitHub Actions               |

_每一项的选择理由与已否决的替代方案见 `docs/adr/`。改动前先读，冲突必须显式提出。_

## 仓库结构（规划）

```
/
├── AGENTS.md
├── CONTEXT.md
├── docs/
│   ├── adr/          架构决策记录
│   ├── agents/       engineering skills 配置
│   └── research/     调研归档
├── backend/          Rust workspace
├── frontend/         Vue 3 + Vite + TS
├── desktop/          Tauri v2 壳
└── deploy/           Compose / Caddy / 运维脚本
```

## 常用命令

> 脚手架建立后补齐本节（全局 AGENTS.md 规定：具体命令属于项目级配置）。

| 目的         | 命令  |
| ------------ | ----- |
| 后端测试     | _TBD_ |
| 后端 lint    | _TBD_ |
| 前端测试     | _TBD_ |
| 前端类型检查 | _TBD_ |
| E2E          | _TBD_ |
| 本地起全栈   | _TBD_ |

## 开发约束（项目特有）

- **不在服务器上编译 Rust**：2C2G 跑 LTO release 构建会 OOM。构建走 CI 或本地，服务器只拉镜像。
- **WebSocket 客户端必须有重连逻辑**：Caddy 重载配置会强制断开全部 WebSocket 连接，这是默认行为。
- **不 mock 数据库做后端测试**：seq 原子分配、唯一约束、分区行为本身就是被测对象。
- **契约只有一份真源**：WS 信封与 REST 契约由 Rust 类型生成 TS 类型，禁止两端各写一遍。
- **秘密聊天不得参与内容审核**：审核只覆盖 Cloud Conversation，代码与文档都不例外。
- **最低 macOS 13**：Tailwind v4 需要 Safari 16.4+，与 Tauri 默认构建目标冲突，必须显式对齐。

## Agent skills

### Issue tracker

Issues and PRDs live as GitHub issues in `Moon-Spirit/jiuyue`, driven by the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its role name (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` at the repo root plus system-wide ADRs in `docs/adr/`. See `docs/agents/domain.md`.
