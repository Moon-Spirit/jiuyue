# JiuYue MVP 规格（Spec v1.0）

> 状态：已定稿（用户授权自治模式，评审由 Momus 代行）
> 共识基础：2026-08-24 用户访谈 12 项决策 + docs/research/ 七份调研报告
> 技术标识符：`jiuyue`

## Problem Statement

目标市场（国内+海外）的普通用户缺少一款**开箱即用、跨全平台、消息可靠不丢、支持可选端到端加密密聊**的现代即时通讯产品。现有方案要么绑定单一生态，要么对多端同步与隐私二者取其一。

## Solution

JiuYue：商业 IM 的 MVP。文字单聊为核心，双通道注册登录，可靠送达与离线同步为基线能力；提供 Telegram 式现代 UI（桌面三栏 / 移动单栏响应式），暗色模式与中英双语；架构上预留群聊、多媒体、音视频、好友关系的扩展位。

## User Stories

1. 作为新用户，我想用邮箱+验证码注册账号，以便没有手机号也能加入。
2. 作为新用户，我想用手机号+短信验证码注册，以便符合国内使用习惯。
3. 作为用户，我想用用户名+密码登录任意设备，以便随时访问我的会话。
4. 作为用户，我想在登录后搜索其他用户的用户名并直接发起聊天，无需好友验证。
5. 作为发送者，我发出的消息在网络抖动时自动重试，且服务端去重保证不会重复出现。
6. 作为接收者，我不在线时收到的消息在重新上线后完整补齐，顺序正确、不丢不错。
7. 作为多设备用户，我在手机和电脑上看到完全一致的会话历史与已读状态。
8. 作为发送者，我想看到消息"已送达/已读"的状态变化，以确认对方收到。
9. 作为任一方，我在输入时对方能看到"正在输入…"提示。
10. 作为发送者，我能撤回两分钟内已发送的消息，双方界面同步移除内容。
11. 作为用户，我能回复引用某条消息，使上下文清晰。
12. 作为用户，我能把一条消息转发给另一个人。
13. 作为注重隐私的用户，我能与对方建立"密聊"会话，消息端到端加密，服务器只见密文。
14. 作为密聊用户，我理解密聊绑定当前设备、换设备后需重新建立（安全码核验防中间人）。
15. 作为暗光环境用户，我想切换深色主题。
16. 作为海外用户，我想把界面切成英文。
17. 作为移动用户，竖屏单栏体验与桌面三栏一致流畅。
18. 作为运维者，我用 docker compose 一条命令拉起全套依赖与服务。

## Implementation Decisions

### 总体

- **Monorepo**：`crates/`（Cargo workspace：`jiuyue-server`、`jiuyue-protocol`、`jiuyue-domain`）+ `app/`（pnpm：Vite7+Vue3+TS 前端与 Tauri2 壳同仓）+ `docs/` + `deploy/`（docker-compose.yml）。
- **后端栈**：axum 0.8 + tokio + PostgreSQL 16 (sqlx, 编译期校验) + Redis 7。前端栈：Vue3 `<script setup>` + Pinia + Tailwind CSS v4 + vue-i18n。
- **实时协议**：WebSocket，JSON 帧 `{v:1, t:"<type>", d:{...}}`，类型常量集中定义于 `jiuyue-protocol` crate 并生成 TS 类型（手写镜像，CI 校验一致性）。二进制帧预留 `v:2`。

### 认证与账号

- 注册双通道：邮箱验证码（开发期 SMTP Mock）/ 手机号短信（开发期固定验证码 `000000`，供应商适配器 trait 化）。
- 统一身份：`users(id UUIDv7, username UNIQUE, display_name)` + 凭证表 `auth_identities(user_id, kind[email|phone], value UNIQUE, verified_at)`；密码 argon2id 哈希。
- 会话令牌：登录签发 access(15m)/refresh(30d) JWT；WS 连接用一次性 ticket（HTTP 换取，5 分钟有效）。
- 设备表：`devices(id, user_id, platform, push_token?, last_seen_at)`，每次 WS 连接 upsert。

### 消息核心（调研定案，见 2026-08-24-axum-im-architecture.md）

- **ID 与排序分离**：消息主键 UUIDv7（应用侧 `Uuid::now_v7`）；会话内单调 `seq BIGINT` 由 `UPDATE conversations SET last_seq=last_seq+1 ... RETURNING` 与消息 INSERT 同事务分配（Telegram pts 模型，gap 合法）。
- **幂等去重**：`UNIQUE(conversation_id, client_msg_id)` + `ON CONFLICT DO NOTHING RETURNING`，冲突返回既有 `(id, seq)` 作 duplicate ACK。
- **投递语义**：persist-then-ack；ACK=已入库。Redis pub/sub 仅作跨实例唤醒通知（可丢，DB 为真相源）；在线状态 `SET presence:{uid} EX 60` 心跳续期；正在输入走 pub/sub 不落库。
- **离线同步**：每设备每会话游标 `member_state(last_delivered_seq, last_read_seq)`；重连/上线发 `sync{cursors}`，服务端按 `seq > cursor` 补发。
- **连接注册表**：`DashMap<UserId, HashMap<DeviceId, ConnHandle>>`，每连接有界 mpsc(512) + `try_send` 背压，重连驱逐幽灵连接（autopush-rs 模式）；心跳 ping 30s/超时 60s，axum 自动 pong。
- **落库加密**：body 列 AES-256-GCM 应用层加密（主密钥环境变量注入，key_id 列留轮换位）。传输 TLS（本地 compose 用自签说明文档）。

### 密聊（M3）

- vodozemac（Apache-2.0，Least Authority 审计）Olm 会话：1v1、设备绑定、不参与云同步（Telegram Secret Chat 模式）；服务端仅中继密文+最小 X3DH prekey 端点（参照 Signal `/v2/keys` 子集）；安全码 = 双方 identity key 指纹比对。

### 扩展预留（本期不实现）

- 群聊：conversations.type 已含 `group` 枚举位；扇出走同一 seq 模型。
- 多媒体：附件元数据列预留；S3 兼容对象存储接口 trait 预留。
- 音视频：WebRTC 信令帧类型预留（详见 2026-08-23-webrtc-tauri.md）。
- 好友关系：直聊默认开放；`relations` 表结构预留 + 用户隐私开关字段。
- 推送：`PushChannel` trait（Apns/Fcm/Mock 三实现），M4 实装。

### UI

- 桌面 ≥1024px 三栏（导航/会话列表/聊天窗）；640–1024 双栏；<640 单栏栈叠。
- Telegram 式视觉基调，品牌主色靛蓝系；暗色模式跟随系统+手动切换；vue-i18n zh-CN/en，默认中文。

## Testing Decisions

- **接缝一（协议层，主）**：真实 axum server + testcontainers PG/Redis，WS 客户端模拟器跑场景——注册→登录→A/B 连接→发消息→ACK→对端接收→断线重连→游标同步→重复 client_msg_id 去重→已读回执→撤回。只断言帧级行为，不断言内部实现。
- **接缝二（领域层）**：纯函数单测——seq 分配器 trait、帧 serde 往返、消息状态机(sending→sent→delivered→read→recalled)、撤回窗口策略。
- **前端**：Vitest 组件冒烟测试（渲染关键组件、store 逻辑），保持轻量。
- **手动 QA（成品门槛）**：`docker compose up` 全链路起服 → HTTP API curl 取证 + websocat 双客户端 WS 会话记录取证 + 浏览器（Vite dev）Playwright 截图取证（三栏桌面/单栏移动视口各一张）。所有证据路径记入交付报告。

## Out of Scope

群聊功能、多媒体消息、音视频通话、好友申请流程、生产部署与域名证书、真机上架材料、内容审核接入、计费体系。（均已在架构留位，二期实施）

## Further Notes

- 生产短信/APNs/FCM 凭证到位前，对应适配器以 Mock 实现，接口签名不变。
- 本地 TLS 自签脚本随 compose 提供；协议版本化从第一帧开始。
