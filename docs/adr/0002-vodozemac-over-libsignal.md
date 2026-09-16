# E2EE 密码学实现选用 vodozemac，排除 libsignal

Status: accepted

Secret Chat 的双棘轮实现选用 **vodozemac**（Matrix 官方 Rust 实现，Apache-2.0，经 Least Authority 审计，`Cargo.toml` 内置 `wasm_js` feature 可编译到 WASM）。**明确排除 libsignal**。

## Considered Options

- **libsignal（Signal 官方）** — **AGPL-3.0**。网络服务一旦使用，AGPL 第 13 条要求向所有使用者提供完整对应源码，对闭源产品是硬性封锁。此外其 npm 包只提供 Windows/macOS/Linux **原生库，浏览器无法运行**，官方声明"在 Signal 之外使用不受支持"。双重不可用。
- **@privacyresearch/libsignal-protocol-typescript** — GPL-3.0，最后发布 2023-05，**上游 GitHub 仓库已 404**。许可证与维护状态双重不可用。
- **matrix-sdk-crypto-wasm** — Apache-2.0 且活跃，但它是**与 Matrix 协议深度耦合**的 OlmMachine（强依赖 Matrix 的 /sync、设备列表、to-device 事件模型），无法用于自定义协议。
- **第三方 vodozemac WASM 封装**（`@dtelecom/vodozemac-wasm`、`@commapp/vodozemac`、`@towns-protocol/vodozemac` 等）— 均可作参考实现，但均为小团队维护、采用量极低，作为长期依赖风险高于自建薄封装。

## Consequences

- 官方 `matrix-org/vodozemac-bindings` **已停止维护**（2024-09 起），浏览器侧需自行编写薄封装（`wasm-bindgen` + `wasm-pack`）。
- 同一份 Rust 密码学核心三处复用：浏览器（WASM）、Tauri 桌面（**直接原生调用，无需 WASM**）、服务端共享类型定义。
- vodozemac 处于 0.x，每个 minor 版本引入破坏性变更。必须**锁死精确版本**并为升级预留预算。
- 若未来需要 group secret chat，vodozemac 的 Megolm 可复用；但密钥分发需自行设计。
