# jiuyue 前端（frontend）

jiuyue 的 Web 客户端（桌面优先，PWA 形态覆盖移动端）。当前处于**仓库骨架阶段**（GitHub issue #2）：只有一个后端连通性检查页，业务功能由后续工单实现。

## 环境要求

- Node.js 24+
- pnpm 12+
- 后端服务默认监听 `http://localhost:8080`（未启动时页面会显示明确的错误状态）

## 常用命令

| 目的       | 命令              |
| ---------- | ----------------- |
| 安装依赖   | `pnpm install`    |
| 开发服务器 | `pnpm dev`        |
| 单元测试   | `pnpm test`       |
| 类型检查   | `pnpm type-check` |
| 生产构建   | `pnpm build`      |
| 预览构建   | `pnpm preview`    |

开发服务器默认运行在 <http://localhost:5173>，并把 `/api/*` 反向代理到 `http://localhost:8080`（去掉 `/api` 前缀）——前端在开发环境也始终同源访问后端，生产环境由反向代理承担同样的职责。

## 目录结构

```
src/
├── api/client.ts           # 类型化 fetch 封装（唯一的 /api 前缀出口）
├── stores/health.ts        # 后端健康状态 store（status / version / loading / error）
├── stores/health.spec.ts   # store 单元测试（在 fetch 边界 mock）
├── views/HealthView.vue    # 连通性检查页（调用 store，展示状态或错误）
├── views/HealthView.spec.ts # 页面渲染测试（后端可用 / 不可达两种状态）
├── router/index.ts         # vue-router 4
├── App.vue                 # 根组件（RouterView）
├── main.ts                 # 应用入口（Pinia + Router）
└── style.css               # Tailwind CSS v4 入口（CSS-first，无 tailwind.config.js）
```

## 技术栈与约束

- Vue 3（`<script setup lang="ts">`）+ Vite + TypeScript（`strict` + `noUncheckedIndexedAccess`）
- Pinia / vue-router 4
- Tailwind CSS v4（CSS-first 配置：`@import "tailwindcss"` + `@tailwindcss/vite` 插件）
- 测试：Vitest + @vue/test-utils（jsdom）
- 不含 UI 组件库；样式一律使用 Tailwind 工具类
- 组件不得直接调用 Tauri / Electron API，桌面能力将来经 `desktop-shell` 接口接入（见 `docs/adr/0008-dual-desktop-shell.md`）
- 不使用容器；不在服务器上编译
