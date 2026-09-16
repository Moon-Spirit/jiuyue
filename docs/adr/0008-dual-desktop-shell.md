# 桌面端双壳：Windows/macOS 用 Tauri，Linux 用 Electron

Status: accepted

Tauri v2 在 Linux 上使用 WebKitGTK，而 WebKitGTK 的 WebRTC 被**编译期开关排除在所有主流发行版构建之外**。因此纯 Tauri 无法满足"Linux 桌面端必须能通话"这条硬需求。决定：桌面端做**两个可互换的壳，共用一份前端** —— Windows/macOS 保持 Tauri v2，Linux 增加同仓 Electron 壳，两者之间隔一层 `desktop-shell` 接口。

## 为什么 Tauri 在 Linux 上无解

WebKitGTK 的 WebRTC 有两把锁，只有一把默认开：

- **编译期**：`ENABLE_MEDIA_STREAM` 默认 ON，但 `ENABLE_WEB_RTC` 默认取 `${ENABLE_EXPERIMENTAL_FEATURES}`，后者默认 OFF。实测 Debian 12/13、Ubuntu 24.04/25.04/25.10、Arch 的打包规则**均未传** `-DENABLE_EXPERIMENTAL_FEATURES=ON`，NixOS 亦为 opt-in。
- **运行期**：`enable-webrtc` 设置默认关闭，而 **wry 全树 grep `webrtc` 零命中** —— Tauri 里没有任何东西会打开它。

`wry#85` 自 2020 年开启、至今 open（最后活动 2026-07-28）；`tauri#13143` 已关闭为 `not_planned`。

Windows（WebView2/Chromium）与 macOS（WKWebView）**都不受此限制**，缺口严格限于 Linux。

## Considered Options

- **自带私有 WebKitGTK / 逐发行版重建** — 否决：需自带 ~100MB+ 私有 WebKitGTK 栈与匹配的 GStreamer 插件，随每个发行版维护一条 ABI 跑步机；不可维护。Dorion 在 NixOS 上正是这么做的，属逐发行版 hack 而非产品策略。
- **Tauri + CEF**（`tauri#14963`，`cef-rs`）— 暂不采用：实验性后端，且 **CEF 默认关闭 WebRTC PipeWire 采集，Wayland 下返回黑帧**（CEF #4053）。保留为将来合并回单壳的路径。
- **Deep link 跳系统浏览器** — 否决：会离开应用，需要会话语义交接、失去集成调用 UI 与通知，且与"通话在原生窗口"的产品要求相冲突。
- **Rust 原生媒体栈**（gstreamer `webrtcbin` / `libdatachannel`）— 否决：ICE/STUN/TURN、编解码、AEC/AGC、设备热插拔，数月媒体工程。
- **等 WebKitGTK** — 否决：无 ETA，2.54 仍为实验性。

## Consequences

- 多一条 CI 流水线，Linux 产物增加 ~100–150MB（AppImage + .deb，x86_64 与 aarch64）；CI 仅在打 tag 时构建 Linux 产物。
- **前端组件不得直接调用 Tauri API**，桌面能力一律经 `desktop-shell` 接口。这既是模块化保证，也是将来若 CEF 成熟、切回单壳的前提。
- 通话运行在**独立的 `BrowserWindow`**（`nodeIntegration: false, contextIsolation: true`），顺带满足"独立 OS 窗口、非浮层"的硬需求。
- 屏幕共享在 Linux 走 `xdg-desktop-portal` + PipeWire（Chromium 110+ 默认启用），因 portal 只暴露单一源，UX 必须围绕**系统选择器**设计而非应用内网格。
- 须有**验证闸门**：在 Ubuntu 24.04/25.10、Fedora 43、Arch 上断言 `RTCPeerConnection` 与 `getUserMedia`/`getDisplayMedia` 存在。不依赖传闻；若某天 Tauri Linux 构建通过该断言，重新评估。
- 调研全文见 [`docs/research/2026-09-16-linux-voice-tauri-vs-electron.md`](../research/2026-09-16-linux-voice-tauri-vs-electron.md)。
