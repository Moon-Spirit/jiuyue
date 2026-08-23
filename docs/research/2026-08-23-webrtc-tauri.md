<!-- project: jiuyue (codename was yueling when written) -->
<!-- extracted from transcript tool_02b1fe20d001Ywa3n1DIrd2r4s, written 2026-08-23 -->

调研完成，报告已写入 `docs\research\webrtc-tauri.md`（262 行，全部结论附一手来源链接 + 查证日期 2026-08-23）。

## 核心发现

**1. 支持矩阵（平台 × 能力）**

| 平台                     | getUserMedia                     | RTCPeerConnection | Insertable Streams       |
| ------------------------ | -------------------------------- | ----------------- | ------------------------ |
| Windows (WebView2)       | ✅ 需处理权限事件                | ✅                | ✅ Chromium≥141 标准 API |
| macOS (WKWebView)        | ✅ macOS 12+                     | ✅                | ✅ 15.4+                 |
| Linux (WebKitGTK)        | ❌ **实际不可用**                | ⚠️                | ❌                       |
| iOS (WKWebView)          | ✅ iOS 14.3+                     | ✅                | ✅ 15.4+                 |
| Android (System WebView) | ✅ 需 manifest 权限 + 宿主授权桥 | ✅                | ✅ WebView≥141           |

- **Linux 是唯一硬伤**：[wry#85](https://github.com/tauri-apps/wry/issues/85) 开了 6 年仍 open，维护者 2026 年直接回复 "no"；出路是正在集成的 CEF（[tauri#14963](https://github.com/tauri-apps/tauri/issues/14963)，活跃中）
- **Android 与 Chrome 浏览器权限模型完全不同**：必须宿主实现 `onPermissionRequest`——wry 已替你实现（`RustWebChromeClient.kt`，借鉴 Capacitor）；缺 `MODIFY_AUDIO_SETTINGS` 会报 `NotReadableError`（[tauri#10846](https://github.com/tauri-apps/tauri/issues/10846)）
- **iOS 残留问题**：麦克风访问 bug [tauri#12068](https://github.com/tauri-apps/tauri/issues/12068) 仍 open，需真机早测

**2. Tauri 权限配置**：wry 0.56.0（PR #1654）新增统一权限 API，Tauri 已暴露 `WebviewBuilder::on_permission_request()`（源码已核实）。macOS 需 Info.plist 两把 key + camera/audio-input entitlements；iOS 需 Info.plist；Android 需 4 条 manifest 权限。高频坑已列表化（含 WebView2 “拒绝后无法重置”的上游缺陷）。

**3. E2EE 论证（最重要的架构结论）**：RFC 8827 4.3 规定 DTLS 握手直接发生在两对端之间、密钥不出端点。**P2P Mesh 下没有媒体服务器，DTLS-SRTP 的传输加密 = 端到端加密**；coturn 只转发密文无法解密。Insertable Streams 是为 SFU/MCU 场景设计的，Mesh 下一期可不做。真正该做的是**把 DTLS 指纹纳入你们已有的安全码核验**，堵住信令服务器主动 MITM 的缺口。

**4. 信令参考**：MDN Signaling 指南（完整 WebSocket 实现）、mediasoup-demo（socket.io 房间模型）、simple-peer（信令解耦抽象）、Perfect negotiation（Mesh 抗 glare 必备）。

**5. 架构建议**：主实现放 **webview JS**（4/5 平台共用一套）；webrtc-rs 不作媒体引擎（纯软编解码，5.1k star vs libwebrtc 差距大）；原生插件仅作移动端性能兜底。基础设施 coturn + 自建信令中继已足够。

风险清单 10 项已入报告，其中需要立即行动的：Linux 一期降级决策、iOS 真机验证、Android 蓝牙路由实测。
