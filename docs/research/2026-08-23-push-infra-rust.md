<!-- project: jiuyue (codename was yueling when written) -->
<!-- extracted from transcript tool_02b1fe0e20015hQLMp3OXxNE9V, written 2026-08-23 -->
调研完成，已写入 `docs/research/push-infra.md`（398 行，9 节，查证日期 2026-08-23）。核心结论：

## 关键发现

**1. APNs crate 推荐：`apns-h2` 0.11.0**
- `a2` 已休眠——现属 reown-com/a2，默认分支最后实质提交 **2024-05**，依赖老化（rustls 0.22）
- `apns-h2` 由 **Threema 官方**出品（E2EE 信使厂商），2026-01 创建、2026-02 发布，依赖新（rustls 0.23/hyper 1.x），token+证书双认证，API 与 a2 同构
- `apns2`（2018 死亡）、`apple-apns`（2023 停滞）排除

**2. iOS 集成：APNs 直通确认（源码级证据）**
- 社区插件 `tauri-plugin-notifications`（Choochmeque，2026-08-18 仍在提交）开启 `push-notifications` feature 后，`registerForPushNotifications()` 直接返回 APNs deviceToken hex
- Swift 源码 swizzle `didRegisterForRemoteNotificationsWithDeviceToken` → `push-token` 事件；静默推送走 `didReceiveRemoteNotification` → `push-message`。**全程无 FCM**

**3. 空推送设计获 Apple 官方背书**
- payload ≤4KB；`content-available=1` 时禁止 alert/badge/sound + `apns-push-type: background`/`priority: 5`
- Apple 明确警告 payload 不放敏感数据——与「空推送+回拉解密」完全一致

**4. ntfy 自托管要点**
- 自托管服务器对 Android **从不走 FCM**；F-Droid 版 app 零 Firebase 且强制 instant delivery
- instant delivery = 前台服务 WebSocket/jsonstream 穿透 Doze，需电池优化白名单
- 发布即一条 POST（JSON 到根 URL 或文本到 topic URL，`Bearer tk_` 认证）
- ⚠️ 重要辨析：ntfy 官方 iOS app 需要 upstream-base-url 转发到 ntfy.sh（隐私弱点）——本架构 iOS 自建 app 走 APNs，不涉及

**5. 备选对比结论**：v1 用独立 ntfy app（零客户端代码）→ v2 内嵌 UnifiedPush connector 品牌化 → 纯自建 WebSocket 仅在硬约束下考虑

文档含完整数据流图（iOS/Android 两张文字版）、环境变量清单、systemd/LimitNOFILE/证书备份等运维交付物 checklist。

<!-- NOTE: the full ~398-line / 9-section report body referenced here (written to docs/research/push-infra.md in the source session) is NOT present verbatim in transcript tool_02b1fe0e20015hQLMp3OXxNE9V; only this final key-findings summary (transcript lines 845-870) and a section outline (lines 760-777) exist. Extraction limited to available verbatim content. -->
