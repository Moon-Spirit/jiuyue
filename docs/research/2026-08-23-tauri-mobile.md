<!-- project: jiuyue (codename was yueling when written) -->
<!-- extracted from transcript tool_02b1fdf6c001YmYgpgwpHnkx2o, written 2026-08-23 -->

# Tauri 2 移动端能力边界调研（加密聊天软件）

> 调研日期：2026-08-23 · 所有版本号当日经 crates.io API 实查 · 一手来源：v2.tauri.app 官方文档、tauri-apps/plugins-workspace 源码（commit `db9c599`）、tauri-apps GitHub issues

## 1. 结论摘要

| 决策点             | 结论                                                                                                                                                                                                                                                           |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ① 密钥安全存储     | **Rust 层直连 OS keychain**：`keyring` v4 生态（`keyring-core` + `apple-native-keyring-store` + `android-native-keyring-store`）。无官方 keychain 插件；`tauri-plugin-stronghold` 移动端成熟度存疑，不作唯一方案。配合官方 `tauri-plugin-biometric` 做解锁门禁 |
| ② 本地 SQLite      | **rusqlite 编译进 Rust 核心层**。官方 `tauri-plugin-sql` 明确不支持 iOS（README 平台表 iOS = ✗）                                                                                                                                                               |
| ③ WebSocket 长连接 | **Rust tokio 层持有连接**，前端经 event 订阅。Android 配合前台服务保活；iOS 放弃常驻（后台 ~30s 冻结），离线消息走 APNs 推送唤醒                                                                                                                               |
| ④ 推送通知         | **官方推送插件不存在**（issue #11651 仍 open）。本地通知用官方 `tauri-plugin-notification`；远程推送必须自写 Swift/Kotlin 原生集成（FCM/APNs）                                                                                                                 |
| ⑤ 权限清单         | 见 §6 汇总表                                                                                                                                                                                                                                                   |

## 2. 平台成熟度

### 当前版本（2026-08-23 crates.io 实查）

> **提取说明（非原文内容）**：原报告共 223 行 / 8 章，但完整正文未随源转录保留——转录中报告正文的写入命令内容被省略，且当时的磁盘原件 `docs/research/tauri-mobile.md` 已不存在。本文件由源转录中幸存的逐字片段拼合而成：① 上方报告开头（标题、来源说明、§1 决策点表、§2 起始标题）；② 附录 A＝作者撰写前的章节内容大纲（覆盖 §3–§8 要点，含少量写作批注，逐字保留）；③ 附录 B＝任务收尾时的最终总结（含完整五决策点证据表）。转录中的写作过程日志（PowerShell 分块写入排错等）未收录。

---

**附录 A · 章节内容大纲（作者写作计划，转录原文逐字保留，覆盖 §3–§8 要点）**

# Tauri 2 移动端能力调研（加密聊天软件架构决策）

- 调研日期 2026-08-23, all versions verified via crates.io API on this date.

## 结论摘要 (table of 5 decisions)

## 平台成熟度

- tauri 2.11.5 (2026-07-01), cli 2.11.4. Mobile stable since 2.0 (Oct 2024).
- plugins-workspace active (last commit 2026-08-13, SHA db9c599)
- Prerequisites: Android Studio + NDK + rustup targets; iOS macOS-only Xcode + CocoaPods + targets (docs updated 2026-08-20)
- High-frequency issues: #15367 WKWebView background resize; #5250 throttling; mobile is production-grade but iOS WebView edge cases remain

## 插件盘点表

| 能力 | crate | 版本 | 移动支持 | 维护状态 | 备注 |

- secure storage: 无官方 keychain 插件；keyring v4 生态 (keyring-core 1.0.0 + apple-native-keyring-store 1.0.2 + android-native-keyring-store 1.0.0)；stronghold 2.3.1 官方但更新停滞(2025-10)且移动端实测存疑
- sql: tauri-plugin-sql 2.4.0 — Android ✓ iOS ✗ (README L10-11)
- notification: 2.3.3 本地通知全平台 ✓；推送无官方支持 (#11651 open)
- deep-link: 2.4.9 全平台 ✓
- barcode-scanner: 2.4.5 仅移动端 ✓
- biometric: 2.3.2 仅移动端 ✓
- websocket: 2.4.2 全平台 ✓
- store: 2.4.4 全平台 ✓
- nfc: 2.3.5 (新插件)
- 社区: tauri-plugin-background-service 1.0.1 (2026-07-30 更新)

## 后台行为

- iOS: 进程挂起 ~30s 后 tokio 冻结（community plugin docs 引用）；BGTaskScheduler 窗口 ~30s/15min+；force-quit 杀死一切 → WS 必断，靠推送唤醒
- Android: 无 FGS 时切后台后进程随时可被杀（Doze/App Standby）；FGS 可保活；Android 15 dataSync 6h 上限；remoteMessaging 类型是聊天应用 Play 政策安全选择
- WebSocket 断连：无论 JS 层还是 Rust 层，进程冻结/被杀都会断；区别在 Android 有 FGS 保活时 Rust 常驻连接可行

## 前台服务

- Tauri 2 无官方 FGS 支持；三种路径：
  1. 社区插件 tauri-plugin-background-service（LifecycleService extends Service, startForegroundService, START_STICKY, stopWithTask=false, boot receiver）
  2. 自写 Kotlin service + manifest merge（develop-mobile 插件开发模式）
  3. 参考社区实现（audio-permissions、timer 插件）
- 权限清单表：FOREGROUND_SERVICE (API28), FOREGROUND_SERVICE_REMOTE_MESSAGING (API34), POST_NOTIFICATIONS (API33), Android 12+ 5s startForeground 限制, Android 15 dataSync 6h timeout

## 限制清单

1. 推送通知无官方插件 (#11651 open, 82 reactions) — 需原生 FCM/APNs 配置
2. tauri-plugin-sql 不支持 iOS (README)
3. iOS 后台 ~30s 冻结 — 无法常驻连接
4. stronghold 移动端成熟度存疑（README 称支持但版本停滞 + issue #2242）
5. iOS WKWebView 后台返回渲染 bug #15367
6. iOS 开发仅限 macOS（Xcode + CocoaPods）
7. 插件不自动注入 iOS Info.plist 权限描述 — 手动添加 NSCameraUsageDescription 等
8. Android 13+ POST_NOTIFICATIONS 运行时权限需请求
9. 桌面 sidecar 模式在移动端不可用（sidecar 是桌面特性）— hmm, did I verify? The docs list sidecar under Develop without platform notes. I shouldn't claim without source. Skip or mark unverified. Actually I know sidecar is desktop-only officially... but MUST NOT speculate. Skip it.
10. Windows 通知仅安装版可用（notification docs notes）— minor, skip for mobile focus. Keep table focused.

## 架构建议

- 密钥存储：Rust 层 keyring-core + apple-native-keyring-store/android-native-keyring-store（libsignal identity keys 存 OS keychain）；biometric 插件做解锁门禁；避免 stronghold 作为唯一方案
- SQLite：rusqlite 编译进 Rust 核心（SQLCipher 或 seequip…不要提未查证的；就说 rusqlite + 可选加密扩展），理由：plugin-sql iOS ✗ + 业务逻辑在 Rust 层（libsignal 解密后落库）
- WebSocket：Rust tokio 常驻持有连接，事件 emit 到前端；Android 用 FGS 保活；iOS 放弃常驻，依赖 APNs 推送唤醒 + 打开时重连同步
- 推送：预留原生层集成（Swift/Kotlin），关注 #11651
- 权限清单汇总表

---

**附录 B · 最终总结（任务收尾答复，转录原文逐字保留，含完整决策点证据表）**

Key findings summary for the response:

1. tauri 2.11.5 (2026-07-01), mobile production-ready ~2 years
2. Critical: tauri-plugin-sql iOS ✗ → rusqlite
3. No official keychain plugin → keyring v4 ecosystem with apple/android store crates
4. Push notifications still open issue #11651
5. iOS background freezes tokio ~30s; Android needs FGS
6. No official FGS support; community plugin or hand-written Kotlin

调研完成，报告已写入 `docs/research/tauri-mobile.md`（223 行 / 8 章节 / 40 处来源链接）。核心结论：

**版本现状**（crates.io 实查，2026-08-23）

- `tauri` **2.11.5**（2026-07-01 发布），移动端自 2024-10 稳定以来已迭代近两年，生产可用；plugins-workspace 最后提交 2026-08-13，活跃维护。

**五个决策点的关键证据**

| 决策点    | 结论                                                                                                                      | 关键证据                                                                                                                                                                                                   |
| --------- | ------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 密钥存储  | Rust 层用 **keyring v4 生态**（keyring-core + `apple-native-keyring-store` 1.0.2 + `android-native-keyring-store` 1.0.0） | Tauri 无官方 keychain 插件；stronghold README 标称全平台 ✓ 但版本停在 2025-10，存疑                                                                                                                        |
| SQLite    | **rusqlite 编译进 Rust**                                                                                                  | [tauri-plugin-sql README L10-11](https://github.com/tauri-apps/plugins-workspace/blob/db9c5998feff9384f9cbbefcbe0d45937c00a1fc/plugins/sql/README.md#L10-L11) 明确 **iOS = ✗**（逐字符核验 U+0078）        |
| WebSocket | **Rust tokio 持有连接**                                                                                                   | iOS 切后台 ~30s 进程冻结（tokio 与 JS 层一起死）；Android 需 FGS 保活                                                                                                                                      |
| 推送      | 本地通知用官方插件；**远程推送无官方支持**                                                                                | [#11651](https://github.com/tauri-apps/tauri/issues/11651) 至今 open（82 👍），FCM/APNs 必须自写原生层                                                                                                     |
| 前台服务  | 无官方方案                                                                                                                | 社区 [`tauri-plugin-background-service`](https://github.com/dardourimohamed/tauri-background-service) v1.0.1 或按官方插件开发模式自写 Kotlin Service（`remoteMessaging` 类型是聊天应用 Play 政策安全选择） |

**两个容易踩的坑**：① Android 15 起 `dataSync` 型 FGS 有 6 小时累计超时，别选它做保活；② iOS 的 Info.plist 条目/entitlements 一律不自动注入，与 Android manifest 自动合并的心智模型不同。

报告内含完整的插件盘点表（12 个官方插件 × 版本 × 移动支持 × 维护状态）、Android/iOS 权限清单、后台行为对照表和分层架构图。
