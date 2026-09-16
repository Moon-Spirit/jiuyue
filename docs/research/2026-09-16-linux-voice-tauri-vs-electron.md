# Linux 桌面端通话能力：Tauri vs Electron（2026-09 结论）

> 调研问题：Tauri v2 桌面端能否在 Linux 上实现语音/视频通话。
> 结论用于 ADR-0008 与规格的桌面端章节。

## 结论（Bottom line）

WebKitGTK **有** WebRTC 实现，但它被**编译期开关排除在所有主流发行版构建之外**，且 wry 从不打开对应的运行时开关。截至 2026-09，Tauri/wry 侧无任何进展（issue 自 2020 年开启，最后活动 2026-07-28）。

**若 Linux 通话是硬需求，不要把规格押在 Tauri + WebKitGTK 上。**

## (a) WebKitGTK 的 WebRTC 支持状态

存在**两把独立的锁**，只有一把默认开启。

**锁 1 — 编译期**（`Source/cmake/OptionsGTK.cmake`）：

```cmake
WEBKIT_OPTION_DEFAULT_PORT_VALUE(ENABLE_MEDIA_STREAM PRIVATE ON)                            # L128
WEBKIT_OPTION_DEFAULT_PORT_VALUE(ENABLE_WEB_RTC    PRIVATE ${ENABLE_EXPERIMENTAL_FEATURES})  # L142
```

`Source/cmake/WebKitFeatures.cmake:222` → `ENABLE_EXPERIMENTAL_FEATURES` 默认 `OFF`。

即：**getUserMedia（MediaStream）= 默认开；RTCPeerConnection（WEB_RTC）= 除非发行版显式传 `-DENABLE_EXPERIMENTAL_FEATURES=ON`，否则关。**

- <https://github.com/WebKit/WebKit/blob/main/Source/cmake/OptionsGTK.cmake#L128>
- <https://github.com/WebKit/WebKit/blob/main/Source/cmake/WebKitFeatures.cmake#L222>

**锁 2 — 运行期**（GLib API `WebKitSettings.cpp`）：`enable-webrtc`（Since 2.38）默认跟随内部 `PeerConnectionEnabled` 偏好；未定义 `ENABLE(WEB_RTC)` 时硬编码 `FALSE`。

**wry 完全不碰这两个设置** —— 在 `tauri-apps/wry` 全树（`792d0359ba6501a4fc360ece17de2ae42329a47c`）grep `webrtc` / `media_stream` / `enable_media`，**零命中**。所以设置默认值说了算，而 Tauri 里没有任何东西会打开它。

WebRTC 于 WebKitGTK **2.38**（2022-09）引入，一直挂在该实验性开关之后。Tauri 维护者 FabianLars：_"initial support was added in 2.38 but it's hidden behind a build flag and so far not a single distro enables it."_

### 发行版实际情况（读的是打包源码，不是博客）

直接下载了真实的 `debian/` 源码包读取 `debian/rules`：

| 发行版 / 版本          | webkit2gtk                                 | 打包中有 `ENABLE_EXPERIMENTAL_FEATURES`? | RTCPeerConnection?  |
| ---------------------- | ------------------------------------------ | ---------------------------------------- | ------------------- |
| Debian 12 bookworm     | 2.50.6-1~deb12u2                           | **无**                                   | ❌                  |
| Debian 13 trixie / sid | 2.52.6-1~deb13u1 / 2.52.6-1                | **无**                                   | ❌                  |
| Debian experimental    | 2.54.0-1~exp1 (2026-09-16)                 | 无                                       | ❌                  |
| Ubuntu 24.04 noble     | 2.52.6-0ubuntu0.24.04.1                    | **无**（`EXTRA_CMAKE_ARGUMENTS`）        | ❌                  |
| Ubuntu 25.04 plucky    | 2.50.4-0ubuntu0.25.04.1                    | 无                                       | ❌                  |
| Ubuntu 25.10 questing  | 2.52.3-0ubuntu0.25.10.1                    | 无                                       | ❌                  |
| Arch（滚动）           | 2.52.6-1                                   | 默认 OFF                                 | ❌                  |
| NixOS                  | webkitgtk_6_0                              | 仅 opt-in：`enableExperimental`          | ❌ 默认             |
| Fedora 41–43           | **未能核实**（src.fedoraproject.org 反爬） | —                                        | **推定 ❌，需实测** |

来源：[Debian tracker](https://tracker.debian.org/pkg/webkit2gtk)（rules 取自 `webkit2gtk_2.52.6-1.debian.tar.xz`）、[Ubuntu Launchpad published sources](https://api.launchpad.net/1.0/ubuntu/+archive/primary?ws.op=getPublishedSources&source_name=webkit2gtk&exact_match=true&status=Published)、[nixpkgs webkitgtk_6_0/package.nix](https://github.com/NixOS/nixpkgs/blob/master/pkgs/by-name/we/webkitgtk_6_0/package.nix)。Yocto/Buildroot 同一默认（`PACKAGECONFIG[webrtc]` 为 opt-in；Buildroot 传 `-DENABLE_WEB_RTC=OFF`）。

> **不要相信关于某个发行版的传闻**，用下面的探针在真实目标发行版上实测。

## (b) Tauri v2 / wry 是否提供开关？自 #13143 / #85 以来有无进展？

**无开关、无 PR、无进展。**

| 项                                                                                      | 状态                                                                             |
| --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| [wry#85 "WebRTC support on Linux"](https://github.com/tauri-apps/wry/issues/85)         | **open**，57 条评论，2020-05-30 创建，最后活动 **2026-07-28**                    |
| [tauri#13143](https://github.com/tauri-apps/tauri/issues/13143)                         | **closed as `not_planned`**（2025-04-05）                                        |
| Tauri 文档 [Webview Versions → Linux](https://v2.tauri.app/reference/webview-versions/) | Linux 表格已过时（最新只到 2.36），无 WebRTC 指引                                |
| Tauri 前置依赖                                                                          | `libwebkit2gtk-4.1-dev` / `webkit2gtk4.1-devel` —— v2 只针对 **4.1（GTK3）** API |

2026 年 wry#85 的评论确认了这堵墙：_"With Tauri/webkit no, with Electron/CEF yes. (With Tauri/CEF probably)"_；另有 _"there is active work on integrating CEF into Tauri (see tauri#14963), and CEF has good WebRTC support on Linux…"_。2023-05 的评论指向社区 NixOS recipe（[discussion 8426](https://github.com/tauri-apps/tauri/discussions/8426)）——通过 `with_webview` 调 `webkit_settings_set_enable_webrtc(TRUE)`。**这仅在发行版编译时开了 `WEB_RTC` 时有效**，即几乎从无效。

Dorion 的 README 说得最准确：_"Support for WebRTC is hidden behind a build-time flag that is unused in most distros, and if it were, the implementation is still incomplete."_

WebKit 侧：[bug 235885](https://bugs.webkit.org/show_bug.cgi?id=235885) 关闭为 **wontfix**，改由 [bug 244004](https://bugs.webkit.org/show_bug.cgi?id=244004) 落地（getDisplayMedia）。WebKit 自家 FAQ 仍称 WebRTC 状态为"进行中"：[WPE FAQ](https://wpewebkit.org/about/faq.html#whats-the-status-regarding-webrtc)。

## (c) 方案与真实成本

| 方案                                                                                                                                                                         | Linux 1:1 通话                      | Linux 群组通话 | Linux 屏幕共享                                                                                                                | 额外成本                                                                                | 风险                                                                     |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| **1. Tauri v2 + WebKitGTK（现状）**                                                                                                                                          | ❌ 发行版构建缺 `RTCPeerConnection` | ❌             | ❌                                                                                                                            | 0                                                                                       | 不满足硬需求                                                             |
| **2. Tauri + 自编译/自带 WebKitGTK**                                                                                                                                         | ⚠️ 有时可用（需 patch wry）         | ⚠️             | ⚠️ Wayland 未验证                                                                                                             | 高：自带 ~100MB+ 私有 WebKitGTK 栈 + 匹配的 GStreamer 插件集；每个发行版一条 ABI 跑步机 | 不可维护，违反"服务器只拉镜像"约束                                       |
| **3. Tauri + CEF**（[tauri#14963](https://github.com/tauri-apps/tauri/issues/14963)，open，72 评论，更新至 2026-08-24；[cef-rs](https://github.com/tauri-apps/cef-rs) 465★） | ✅（Chromium）                      | ✅             | ⚠️ **CEF 默认关闭 WebRTC PipeWire 采集** → Wayland 上黑帧（[CEF #4053](https://github.com/chromiumembedded/cef/issues/4053)） | 高：+100–150MB CEF 运行时；实验性后端                                                   | 未到可排期程度，只做 spike                                               |
| **4. 仅 Linux 用 Electron 壳**（同一份 Vue 应用）                                                                                                                            | ✅ 成熟                             | ✅             | ✅ PipeWire portal，默认开                                                                                                    | 中：第二条构建流水线；产物 ~100–150MB；两个壳需同步                                     | 体积；必须保持前端与壳无关                                               |
| **5. Deep link 跳系统浏览器**                                                                                                                                                | ✅                                  | ✅             | ✅                                                                                                                            | 开发成本低，**产品成本高**                                                              | 离开应用：会话语义交接、无集成调用 UI/通知，与"通话在原生窗口"的精神冲突 |
| **6. Rust 原生媒体（gstreamer `webrtcbin` / `libdatachannel`）**                                                                                                             | ✅                                  | ✅             | ⚠️需自己实现 portal ScreenCast                                                                                                | **极高**：ICE/STUN/TURN、编解码、AEC/AGC、设备热插拔 —— 数月媒体工程                    | 小团队不可行                                                             |

**好消息**：缺口仅在 Linux。Windows Tauri = WebView2（Chromium）✅；macOS Tauri = WKWebView 支持 WebRTC ✅（需在 Info.plist 加 `NSCameraUsageDescription` / `NSMicrophoneUsageDescription`）。所以"仅 Linux 换壳"是一个外科手术式的、可辩护的决定。

## (d) 屏幕共享：Wayland vs X11

- **Electron / Chromium**：走 `xdg-desktop-portal` ScreenCast → Wayland 上 PipeWire（X11 上走 X11/xWayland）。`#enable-webrtc-pipewire-capturer` **自 Chromium 110 起默认启用**（_"No action is required in Chrome versions 110 and greater"_，[Red Hat KB 6712111](https://access.redhat.com/solutions/6712111)）；Electron 44 远在其后。注意官方限制：_"desktopCapturer.getSources() only returns a single source on Linux when using Pipewire"_ —— 交由系统选择器决定，UX 要围绕**系统选择器**设计（Electron 提供 `useSystemPicker`），而不是应用内网格。
- **WebKitGTK**：`getDisplayMedia` 在引擎里存在（bug 244004 已落地），但因同样的 `WEB_RTC`/`MEDIA_STREAM` 原因在发行版构建中不可达；Tauri/wry 没有 portal 选择器的管线，也没有任何已发布的 Tauri 应用报告过 Linux 屏幕共享可用。
- **CEF**：Wayland 上返回**黑帧**，除非以 `rtc_use_pipewire` 重建并 `set_allow_pipewire(true)`。

## (e) 真实应用的选择

| 应用                              | Linux 上的壳                                                                                                                                         | 原因 / 证据                                                        |
| --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| **Element Desktop**               | **Electron 44.2.0**                                                                                                                                  | 通话 + 屏幕共享在 Linux 上只有 Chromium 可行                       |
| **Signal Desktop**                | **Electron**                                                                                                                                         | E2EE 通话 + 屏幕共享                                               |
| **Revolt Desktop**                | 今天 **Electron**；README 称将迁移到 Tauri（[stoatchat/for-web#14](https://github.com/stoatchat/for-web/issues/14) 仍 open）                         | 其 Tauri 重写正是这个陷阱：值得观察，但无证据表明 Linux 语音已解决 |
| **Dorion**（Tauri，Discord 包装） | **Tauri**，README 功能表 Linux **Voice = "?"**；NixOS 甚至从源码重建 WebKitGTK（[nixpkgs PR #265771](https://github.com/NixOS/nixpkgs/pull/265771)） | 逐发行版 hack，不是产品策略                                        |

**规律：凡在 Linux 上有语音的应用，都发行 Chromium（Electron/CEF）。** 唯一在 Tauri 上尝试 Linux 语音的，都是针对特定发行版重建 WebKitGTK。

## 建议

1. **桌面端做两个可互换的壳，共用一份前端**，中间隔一层薄接口 `desktop-shell`（`startCall()` / `openCallWindow()` / 屏幕共享选择器 / 通知与权限管线）：
   - Windows + macOS：**Tauri v2**（不变，WebView2/WKWebView 的 WebRTC 可用）
   - Linux：同仓 **Electron 壳**（`desktop/electron/`），复用已构建的 Vue bundle；通话跑在**独立的 `BrowserWindow`**（`nodeIntegration: false, contextIsolation: true`）—— 同时满足"独立 OS 窗口、非浮层"的硬需求，并获得可用的 1:1、群组（WebRTC 到 LiveKit）与 PipeWire 屏幕共享
   - 前端纪律：**通话/聊天组件内不得直接调用 Tauri API**，只能经由 shell 接口。这是模块化保证，也是将来切到方案 3 的前提
2. **接受的成本**：多一条 CI job + 一个 ~100–150MB 的 Linux 产物（AppImage + .deb，x86_64 与 aarch64），CI 仅在打 tag 时构建
3. **保留合并的路**：对 **Tauri + CEF**（cef-rs）做限时 spike，且要求该 spike 同时证明 Wayland 屏幕共享（CEF #4053 表明不做定制构建就不行）
4. **兜底**：若 Linux Electron 壳延期，退回方案 5（deep link 到系统浏览器）—— 最便宜但会离开应用，需明确的产品方签字
5. **加验证闸门**（不靠传闻）：在 Ubuntu 24.04 LTS、25.10、Fedora 43、Arch 上手动跑一次冒烟断言：

```js
typeof RTCPeerConnection !== "undefined" &&
  !!navigator.mediaDevices?.getUserMedia &&
  !!navigator.mediaDevices?.getDisplayMedia;
```

并记录 WebKitGTK/Chromium 版本。若某天 Tauri Linux 构建在目标发行版上通过该断言，再回来重新考虑方案 1。

**已否决**：方案 2（自带私有 WebKitGTK —— 不可维护，且违反"服务器只拉镜像"）、方案 6（原生媒体栈 —— 数月工程量，不是首发功能）、以及"等 WebKitGTK"（无 ETA，2.54 仍为实验性）。

**标注的不确定性**：未能读取 Fedora 的 `webkit2gtk4.1.spec`（站点反爬），Fedora 按"推定禁用 + 用探针实测"处理。早前第三方报告称 Fedora 39 可部分采集媒体，这与 `ENABLE_MEDIA_STREAM=ON` 一致，**不代表 `RTCPeerConnection` 可用**。
