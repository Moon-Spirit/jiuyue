# JiuYue 调研索引

> 提取自 librarian 子代理会话转录，2026-08-24 落盘。两份 2026-08-23 报告原始全文未随转录留存，落盘版本为其最终结论摘要（文内已标注）。

| 文件                                                                         | 主题                                                                                    | 查证日期   | 形态     |
| ---------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | ---------- | -------- |
| [2026-08-24-axum-im-architecture.md](./2026-08-24-axum-im-architecture.md)   | axum+PG+Redis IM 参考架构：连接注册表/可靠投递/Redis 边界/库表设计                      | 2026-08-24 | 完整报告 |
| [2026-08-24-e2ee-crypto-landscape.md](./2026-08-24-e2ee-crypto-landscape.md) | E2EE 库全景：libsignal(AGPL)/proteus(GPL)/OpenMLS(MIT)/vodozemac(Apache) 对比与选型裁决 | 2026-08-24 | 完整报告 |
| [2026-08-24-tauri-mobile-push.md](./2026-08-24-tauri-mobile-push.md)         | Tauri2 移动端成熟度+离线推送方案+中国厂商通道现状                                       | 2026-08-24 | 完整报告 |
| [2026-08-23-libsignal-deep-dive.md](./2026-08-23-libsignal-deep-dive.md)     | libsignal 深潜：crate 结构/API 形态/许可证风险                                          | 2026-08-23 | 结论摘要 |
| [2026-08-23-webrtc-tauri.md](./2026-08-23-webrtc-tauri.md)                   | Tauri 全平台 WebRTC 能力矩阵/E2EE 论证/信令参考（二期音视频用）                         | 2026-08-23 | 结论摘要 |
| [2026-08-23-push-infra-rust.md](./2026-08-23-push-infra-rust.md)             | 推送基建：apns-h2 选型/iOS 直通/ntfy 备选对比                                           | 2026-08-23 | 结论摘要 |
| [2026-08-23-tauri-mobile.md](./2026-08-23-tauri-mobile.md)                   | Tauri 移动端决策表：后台行为/前台服务/插件盘点/权限清单                                 | 2026-08-23 | 完整报告 |

**关键裁决速览**：密聊 E2EE 用 vodozemac（闭源商业约束下唯一许可安全且经审计的选项）；M1 移动端=工程壳+推送 spike（桌面优先）；推送抽象 `PushChannel` trait 三实现（APNs/FCM/Mock）；消息核心走 UUIDv7+会话内 seq+persist-then-ack。
