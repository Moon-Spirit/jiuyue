# 前端技术栈

Status: accepted

前端采用 Vue 3 + Vite + TypeScript + Pinia 4。UI 以**自定义聊天组件**为主：Tailwind v4 负责样式、Reka UI 提供无样式原语、shadcn-vue 提供复制式组件；**Naive UI 仅用于管理后台**（它不注入全局样式，与 Tailwind 的 preflight 不冲突，而 Element Plus 会）。消息列表使用 **virtua**——调研中唯一原生支持"反向滚动 + 动态高度"的成熟虚拟滚动方案。PWA 用 vite-plugin-pwa 的 `injectManifest` 策略（推送与通知点击需要自定义 Service Worker）。桌面端用 Tauri v2 + 官方插件。

## Considered Options

- **Element Plus 全站** — 被否决：注入全局样式与 Tailwind preflight 冲突，且聊天 UI 几乎全部自定义，组件库价值有限；仅在管理后台的数据表格场景仍有价值。
- **vue-virtual-scroller** — 被否决：其"反向垂直滚动 + 无限滚动"至今仍是未实现的需求（issue 未关闭），而这是聊天消息列表的核心交互。
- **UnoCSS 替代 Tailwind** — 非首选：UnoCSS 仍在维护（wind4 preset 可模拟 Tailwind v4），但 Tailwind v4 已追平其速度优势，且生态（插件、IDE 工具、组件库）更大。仅在需要 attributify 等特性时才考虑。

## Consequences

- **最低 macOS 版本定为 macOS 13。** Tailwind v4 依赖 `@property` 与 `color-mix()`，要求 Safari 16.4+；而 Tauri 官方 Vite 模板默认 `build.target = safari13`。二者必须显式对齐，否则旧系统 WebView 上样式会静默失效。
- **Linux 桌面端不能通话。** WebKitGTK 中 `RTCPeerConnection` 不存在（非配置问题，是引擎缺失）。Linux 用户需要使用浏览器版本进行音视频。
- **Windows 桌面端的通话必须显式处理权限回调**，否则 `getUserMedia` 会被静默拒绝；屏幕共享存在已知的 WebView2 死锁缺陷，需要早期专项验证。
- 秘密聊天的密码学在 Tauri 端走**原生 Rust 调用**（无需 WASM），在浏览器端走 WASM；两端复用同一份 Rust 核心（见 ADR-0002）。
