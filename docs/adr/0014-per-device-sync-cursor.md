# 每设备同步游标：持久化 + 合并写入 + 有界前向补齐

Status: accepted

ADR-0013 把断线游标留在了**连接层**（内存、随连接死亡），并把「服务端持久化每设备 Sync Cursor」显式暂缓给后续工单。本 ADR 落地该工单（#13），把 **Sync Cursor 变成服务端持久化的、按 Device 的状态**，使一台设备在长时间离开后（合上笔记本、重启进程、重新加载页面）能被**准确告知它错过了什么**，而不必重读整个会话。

## 决策

- **游标按 Device × Conversation 持久化。** 新表 `sync_cursors(session_id, conversation_id, last_seq, updated_at)`，主键 `(session_id, conversation_id)`。`session_id` 就是 `sessions` 表——CONTEXT.md 里的 **Device**——所以「每个 Device 独立推进自己的游标」是**模式事实**，不是约定。`last_seq` 是会话内 Sequence Number（ADR-0003 的唯一排序权威）：`last_seq` 表示「该 Device 至少持有 seq 1..=last_seq」。
- **游标只前进，不回退。** 落库语句取 `GREATEST(existing, incoming)`，即使同一 Device 的两个连接乱序上报、或上报被重放，也不会把位置往回写。`last_seq = 0` 是「无记录」值，不落库（`CHECK (last_seq >= 1)`）。
- **接入现有补齐原语，不另造旁路。** 返回的游标通过既有前向游标 `GET /conversations/{id}/messages?after=` 逐页补齐；`after` 仍是 ADR-0012 的 `seq` 权威。**没有**第二条补齐路径，也没有第二份排序真源。
- **服务端把位置告诉设备，而不是把错过的消息推给设备。** 连接建立时，若该 Device 有已存游标，服务端在开场心跳之后立即推 `ServerEvent::SyncState`，**只带位置、不带消息**。因此该帧大小被 Device 的会话数界定；一次「离开一个月」的补齐永远是**有界分页**（每页 ≤ 100）。Device 没有存过游标时**不发**该帧，老客户端行为不变。
- **写入由进程合并，而不是由客户端节制。** 客户端用 `ClientEvent::SyncCursor` 上报消费进度；连接层在内存里**按 Conversation 合并**（一台设备一个会话至多一条，取最大），只在**心跳、连接拆除、或待写集合超过 `CURSOR_CHECKPOINT_BATCH`(64)** 时落库。一次 flush 对每个会话至多一条语句。于是无论客户端多话痨，都不会退化成「每消息每设备一次写」。
- **投递语义仍是 at-least-once。** 补齐会重发客户端已有的消息，因此**客户端必须按 Message ID 幂等应用**（重复是无害 no-op）。这一条写进契约文档（`SyncState` / `SyncCursor` / `NewMessage`），多设备路径同样遵守。

## 合并写入的取舍（写盘时机与丢失代价）

游标不是每消息一写，而是合并在内存、按上述三个时机落库：

- **心跳**（默认 30s）：长时间在线的连接也最多积累一个心跳周期的未落盘进度。
- **连接拆除**：正常断开（刷新页面、关闭浏览器、服务端主动关闭）时兜底 flush，正常路径**零丢失**。
- **批量阈值**：一台设备在两个心跳之间触及 64 个不同会话时提前落库，内存中的待写 map 不至于无界。

**进程在两次 checkpoint 之间被强杀**，代价是该 Device **至多一个心跳周期**的进度未落盘；它下次连接会拿到稍旧的游标，重新拉取这一小段。因为投递是 at-least-once 且应用按 Message ID 幂等，重复拉取是无害的——**用重复换「绝不丢」**，与 ADR-0003 一致。

## Considered Options

- **游标留在客户端（localStorage）** — 否决：CONTEXT.md 明确 Sync Cursor 按 Device 且需服务端可回答；客户端本地存储不能覆盖「换台机器 / 重装 / 新客户端登录」，也不能让服务端在连接时主动告知缺口。
- **每条消息都写一次游标** — 否决：2 vCPU / 2 GB 单机上的写放大灾难；本 ADR 的合并/checkpoint 就是为消灭它。
- **按连接而非按 Device 存游标** — 否决：连接不代表设备身份；重连即新连接，位置无法跨连接证明（这正是 ADR-0013 的 `unavailable`）。
- **按 User 存游标** — 否决：会把「手机的进度」当成「笔记本的进度」，一台设备的推进掩盖另一台设备的缺口。
- **服务端直接把错过的消息集合作为一次响应下发** — 否决：一次「离开一个月」会退化成无界响应；位置 + 既有分页补齐才是有界的。
- **exactly-once / 每设备确认每消息** — 否决：代价与复杂度远超收益，且与 ADR-0003 的至少一次语义冲突。

## Consequences

- 契约新增 `SyncCursor`、`SyncState`，以及 `ServerEvent::SyncState`、`ClientEvent::SyncCursor`；全部是**增量**变更，未知事件类型被老客户端跳过（ADR-0003）。前端类型由 `scripts/gen-contract.ps1` 生成并提交，不手写副本。
- `sync_cursors.session_id` 外键指向 `sessions`，`conversation_id` 外键指向 `conversations`，两者都 `ON DELETE CASCADE`：登出 / 撤销会话 / 删除账号 / 删除会话都会带走对应游标，不留孤儿。
- 会话层前向游标（ADR-0013）与每设备游标（本 ADR）是两层，各司其职：前者回答「这条连接缺了什么」，后者回答「这台设备消费到哪」。两者都落到同一个 `seq` 权威。
- 已读标记 / 未读数是 **User** 维度、由 #16 负责；本 ADR 不实现，也不与之混用。Sync Cursor 与 Read Marker 是两个概念（CONTEXT.md）。
- 连接入口现在绑定 **Device**（`user_id` + `session_id`），不再只绑 User；这为后续设备列表、逐设备撤销、逐设备推送留好了接缝。
