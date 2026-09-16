# 断线重连与补洞：连接级握手 + 会话级前向游标

Status: accepted

断线恢复分两层，各管各的缺口，且都必须**显式**：

- **连接层**：信封上的 `s` 是**每连接**单调序号。客户端一旦发现 `s` 跳号，或心跳迟到，**不得静默继续**；它发送 `Resume{last_seq, connection_id}` 握手，服务端用**有界回放缓冲**回答 `Resync{reason}`：
  - `fresh` — 新连接，s 语义上无可补；
  - `replayed` — 缺口仍在缓冲里，已按原 `s` 逐条重放，重放发生在 `Resync` 之前；
  - `unavailable` — 位置不可证明（新连接、`connection_id` 不匹配，或已被淘汰），客户端必须去会话层补洞。
- **会话层**：`GET /conversations/{id}/messages` 在 ADR-0012 的 `before`（向后翻）之外新增 `after`（向前补，排他，返回 `seq > after` 的**最旧**一页）；响应新增 `next_after`，`has_more` 表示"所请求方向上还有下一页"。重连后客户端从自己持有的会话游标 `after` 逐页前拉到不再有。会话游标**只有一份真源**：ADR-0012 的会话内 `seq`，不另造平行体系。

投递语义是 **at-least-once**：重放会重发客户端已有的消息，所以客户端必须**按 Message ID 幂等应用**（重复到达是无害的 no-op）。**不做 exactly-once**——那是重复换来的"绝不丢"。

## 断线健壮性

- 服务端按 `HEARTBEAT_INTERVAL`（默认 30s）发 `Ping`。TCP 半开连接与健康连接在字节层无法区分，只有周期性流量能暴露它。
- 客户端在 `staleAfterMs`（默认 75s = 2.5 个心跳）内未收到任何信封即判定连接已死，主动关闭并走重连。
- 重连退避为**指数 + 减式抖动**：`delay = min(maxDelay, initial * factor^attempt) * (1 - jitter * random())`，默认 0.5s 起、30s 封顶、抖动 0.5。Caddy reload 会同时切断**所有** WebSocket（`AGENTS.md`），同步重连是真实场景而非假想，抖动把惊群摊开。
- `online` 事件与标签页重新可见时**立即重试**，放弃退避——平台已经告诉我们等待的理由消失了。

## 回放缓冲上界

每连接保留最近 **128** 个信封（`DEFAULT_REPLAY_CAPACITY`），环形淘汰最旧的。理由：

1. 2 vCPU / 2 GB 单机（ADR-0005、ADR-0010），**无界**按连接积压就是内存炸弹。
2. 128 个小 JSON 信封是**每连接几十 KB**量级，几百个并发连接也只有几 MB。
3. 它大于注册表扇出队列的 64（`CONTROL_QUEUE_CAPACITY`）——那正是信封被丢弃的唯一来源。短暂停止消费的客户端总能被回放补上，无需降级到会话层。
4. 淘汰是**正确降级而非丢数据**：下界之外的位置回答 `unavailable`，客户端从会话游标精确补齐。持久记录是 `messages` 表，不是回放缓冲。

## Considered Options

- **exactly-once** — 否决：分布式下需要每消息确认与去重状态，代价与复杂度远超收益；且与 ADR-0003 的"回放 + 至少一次"语义冲突。
- **无界回放缓冲** — 否决：单机内存炸弹。
- **只做 REST 全量重拉** — 否决：会话量大时代价随规模增长，且在 2GB 机器上不可接受。
- **为补洞新造一套游标/端点** — 否决：会与 ADR-0012 的 `seq` 权威漂移，契约出现两份真源。
- **服务端持久化每设备 Sync Cursor** — 暂缓：跨刷新/多设备一致性属于后续工单；本版游标由客户端持有（CONTEXT.md 的 Sync Cursor 按 Device），刷新后的持久化另议。

## Consequences

- 契约新增 `ClientEvent::Resume`、`ServerEvent::Resync`（含 `ResyncReason`），以及 `MessagePageQuery.after`、`MessageList.next_after`；全部是**增量**变更，旧客户端跳过未知事件类型即可（ADR-0003）。
- `Ping` 新增 `connection_id`（服务端心跳携带，客户端心跳为 `null`）：这是服务端区分"本连接内可回放"与"上一连接的旧位置，不可证明"的依据。
- 客户端必须能接受并幂等吸收重复的 `NewMessage` / `MessageAck`（按 Message ID，回退按 Client Message ID）。
- 客户端在**每次重连**以及**每次检测到 `s` 缺口**时都触发一次会话层修复；修复经过合流（coalesce），多次触发合并为一次遍历。
- 本版会话游标是**内存态**，跨页面刷新不保留；刷新后退化为重新拉取最新页，这与"重连无需手动刷新"（socket 自动重连）不是同一件事。
