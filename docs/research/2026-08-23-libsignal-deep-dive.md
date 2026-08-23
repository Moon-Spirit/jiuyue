<!-- project: jiuyue (codename was yueling when written) -->
<!-- extracted from transcript tool_02b208cf0001skhH4FrnOKxob1, written 2026-08-23 -->

> **提取状态说明（2026-08-24，截断声明）**：源转储为思考/文本流，**未包含完整 273 行报告正文**。正文当时由原会话经 bash 分块写入 `docs/research/libsignal.md`，该文件现已不存在，opencode 会话存储与同级转储中亦无工具调用载荷，正文不可恢复。以下内容为源文件**逐字拷贝**的三段可恢复材料：① 报告骨架（源 L538–L581）；② 最终答复的关键发现摘要（源 L739–L758）；③ 写作前引用清单草稿（源 L595–L636）。缺失部分：§2 表格全文、§3 API 各小节详情、§4 案例细节、§5 依赖树分析、§6 许可证细节、§7 风险清单 R1–R10 全文、附录。

# libsignal Rust 生态调研（查证日期：2026-08-23）

## 0. 调研方法与时效声明

- 源码基于 signalapp/libsignal main @ b056faa6dd02961cff24064c54c089c52e1a0753（2026-08-19 前后，与 v0.101.0 release 同版本）
- 所有链接为 SHA 固定永久链接
- 标注【官方】= 官方仓库/文档直接证据；【推断】= 由证据推理

## 1. 结论摘要

- 官方 crate 未发布 crates.io → git tag/rev 依赖
- crates.io 上 libsignal-protocol 是 2019 第三方占位
- API 已大改：无 SessionCipher 结构体；异步自由函数 + async_trait(?Send) store traits；PQXDH（含 Kyber prekey）
- AGPL-3.0-only —— 商业分发强传染
- 移动端可行：protocol crate 纯 Rust；官方 CI 构建 iOS/Android
- 真实集成案例 ≥4 个

## 2. crate 状态

表格：名称/状态/证据

## 3. API 形态

3.1 Store traits（6 个 trait 全列 + ProtocolStore blanket）
3.2 会话建立 process_prekey_bundle / process_prekey
3.3 加解密 message_encrypt/message_decrypt（无 SessionCipher 类）
3.4 注册流程（identity/signed prekey/kyber prekey/one-time prekey 生成 + PreKeyBundle::new 上传形态）
3.5 群聊 Sender Keys API
3.6 多设备扇出 sealed_sender_multi_recipient_encrypt
3.7 参考实现 InMemSignalProtocolStore

## 4. 集成模式（真实案例）

表格 + 每个案例细节（依赖方式、pin 策略、坑）

## 5. 移动端编译

- 依赖树分析（纯 Rust 证据链）
- protoc 构建要求
- nightly 工具链 vs crate MSRV
- 官方 CI 证据
- boring 不需要（net 才用）
- dalek fork 时间线（v0.94.4 需要 → v0.101.0 不需要）
- wasm 先例

## 6. 许可证

AGPL-3.0-only 细节与影响

## 7. 风险清单

编号列表，每条带缓解措施

---

<!-- 以下为源 L739–L758 逐字拷贝：原会话最终答复的关键发现摘要 -->

## 关键发现

**① crate 状态：未发布，只能 git 依赖**

- crates.io 上的 [`libsignal-protocol`](https://crates.io/crates/libsignal-protocol) 是 **2019 年第三方死包**（包装 C 库，v0.1.0 停更），名字已被占用；`libsignal-protocol-rust` 不存在（404）
- 官方发布请求 [Issue #490](https://github.com/signalapp/libsignal/issues/490) 自 2022-10 至今 open；最新 release 为 [v0.101.0](https://github.com/signalapp/libsignal/releases/tag/v0.101.0)（2026-08-14）
- 正确姿势：`libsignal-protocol = { git = "https://github.com/signalapp/libsignal", rev = "<sha>" }`

**② API 形态与训练记忆差异巨大（重要）**

- **SessionCipher 结构体已被移除**——旧文件自述为"frozen snapshot"仅用于测试（[session_cipher_legacy.rs](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/rust/protocol/src/session_cipher_legacy.rs#L1-L14)）；现行入口是自由函数 `message_encrypt`/`message_decrypt`
- Store traits 全部 `#[async_trait(?Send)]`，且比经典 X3DH 多出 **KyberPreKeyStore**（PQXDH）和 SenderKeyStore，共 6 个维度（[traits.rs](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/rust/protocol/src/storage/traits.rs#L43-L186)），官方有 `InMemSignalProtocolStore` 参考实现
- 注册 bundle 必须含 kyber prekey 三元组（[PreKeyBundle::new](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/rust/protocol/src/state/bundle.rs#L137-L148)，10 参数）
- 群聊 = 4 个自由函数按 `(sender, distribution_id: Uuid)` 索引；多设备扇出直接用 [`sealed_sender_multi_recipient_encrypt`](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/rust/protocol/src/sealed_sender.rs#L1362-L1385)

**③ 集成案例（4+ 个源码级证据）**：gurk-rs→presage→libsignal-service-rs 链、aparte、almet/signal-without-smartphone、getmaapp/signal-wasm（后者 pin 的正是本次基线 commit 且跑在 wasm32 上）

**④ 移动端**：protocol crate 依赖链**纯 Rust**（BoringSSL 只在 dev-deps 和 net crate），官方 CI 构建 iOS/Android 目标；坑是 build.rs 需要 `protoc`。旧版需 patch curve25519-dalek fork，v0.101.0 已不需要

**⑤ 许可证：AGPL-3.0-only —— 一票否决项**。技术上完全可行，但若产品不做整体 AGPL 开源，libsignal 不可用，应转向 vodozemac/openmls（其许可证与 API 我未查证，已在报告 标注为待调研项）。

报告末尾附 10 条风险清单（R1 许可证阻断级 / R2 零稳定性承诺 / R6 `?Send` future 线程模型约束等），可直接作为架构评审输入。

---

<!-- 以下为源 L595–L636 逐字拷贝：原会话写作前的引用清单草稿（正文各节引用的完整证据索引） -->

SHA = b056faa6dd02961cff24064c54c089c52e1a0753
Base permalink: https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/

Files:

- Cargo.toml#L44-L48 (workspace version/license)
- Cargo.toml#L92-L97 (forks boring-signal/spqr)
- Cargo.toml#L240-L244 ([patch.crates-io] boring only)
- rust/protocol/Cargo.toml#L6-L12, #L17-L56
- rust/core/Cargo.toml (boring dev-deps — lines ~30-36; I saw content but not exact line numbers in output... The core Cargo.toml dev-dependencies section: from my read, [dev-dependencies] appears after deps; boring/boring-sys listed there. I don't have exact line numbers. I can cite file path without line anchor or use approximate. Better cite without #L for that one, or reference "dev-dependencies 段". For rigor I'll link the file and name the section.
- rust/crypto/Cargo.toml (pure deps)
- rust/protocol/src/storage/traits.rs#L48-L186
- rust/protocol/src/session_management.rs#L43-L51, #L158-L168
- rust/protocol/src/session.rs#L181-L189
- rust/protocol/src/group_cipher.rs#L16-L22, L124, L196, L228
- rust/protocol/src/sealed_sender.rs#L1362-L1385
- rust/protocol/src/state/bundle.rs#L125-L148
- rust/protocol/src/kem.rs#L201, L432-L439
- rust/core/src/curve.rs#L275, L362 (calculate_signature) — wait curve.rs is in libsignal-core? The grep said filename curve.rs — path was rust\core\src\*.rs so yes rust/core/src/curve.rs#L275-L283, #L362
- rust/protocol/src/identity_key.rs#L126 (IdentityKeyPair::generate)
- rust/protocol/src/lib.rs#L51-L95 (exports), #L34 (pub mod pqxdh)
- rust/protocol/src/session_cipher_legacy.rs#L1-L14
- rust/protocol/build.rs (protoc)
- rust-toolchain (nightly-2026-07-15)
- .github/workflows/android_integration.yml, ios_artifacts.yml
- storage/inmem.rs#L383-L395, #L557

External:

- https://crates.io/crates/libsignal-protocol
- https://crates.io/api/v1/crates/libsignal-protocol-rust → 404
- https://github.com/signalapp/libsignal/issues/490
- https://github.com/signalapp/libsignal/issues/626
- https://github.com/signalapp/libsignal/releases/tag/v0.101.0
- https://github.com/whisperfish/presage/blob/main/presage/Cargo.toml
- https://github.com/boxdot/gurk-rs/blob/master/Cargo.toml
- https://github.com/paulfariello/aparte/blob/develop/Cargo.toml
- https://github.com/almet/signal-without-smartphone/blob/main/Cargo.toml
- https://github.com/getmaapp/signal-wasm/blob/main/Cargo.toml
- https://github.com/signalapp/ringrtc/blob/main/src/rust/Cargo.toml
- https://github.com/signalapp/Signal-Calling-Service/blob/main/frontend/Cargo.toml
- https://raw.githubusercontent.com/signalapp/libsignal/v0.94.4/Cargo.toml
- spqr: https://github.com/signalapp/SparsePostQuantumRatchet/blob/v1.5.3/Cargo.toml
- README.md (build requirements)
