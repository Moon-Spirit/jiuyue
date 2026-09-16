# 消息投递协议：全局标识 + 会话内 seq + 连接级回放

Status: accepted

投递协议采用三层标识与一套回放机制：**全局 Message ID**（时间可排序，用于身份与去重）、**会话内单调 seq**（Conversation 内排序与缺口检测的唯一权威）、**连接级 connection_id + seq**（Mattermost 式可靠 WebSocket 回放）。客户端断线后携带上次消费位置重连，服务端从队列回放；若序号出现断层，客户端立即断线重连而非静默跳过。

## Considered Options

- **纯时间戳排序** — 被否决：客户端时钟不可信，且同毫秒并发无法定序。
- **单一全局序列（全库单调递增）** — 被否决：全局序列在分片/多写入者下成为瓶颈，且强迫所有会话共享排序，无法按会话隔离扩容。
- **仅靠 REST 拉取、放弃连接级回放** — 被否决：WebSocket 断线期间的窗口必然丢事件，只能整体重新拉取，代价随会话量增长。

## 参考的既有实现

- **Mattermost 可靠 WebSocket** — 客户端保存 `connection_id` 与 `sequence`；服务端按 (user, connection) 保留队列；序号不匹配即断开并与服务端重新同步。取此模型作为连接层基础。
- **Telegram pts/seq** — 按"消息箱"维护独立 pts，收到 `pts > local + count` 即判定缺口并调用 getDifference；等待 0.5s 容忍乱序。取此作为**缺口检测算术**。
- **Matrix /sync** — `limited` 标志 + `prev_batch` 分页补齐，`to_device` 直到 ack 才删除。取此作为**补齐路径**与"事件至少一次"的语义。
- **Signal Sesame** — 每设备独立信箱、取走即删、重试请求与投递回执。取其**每设备独立队列**的思路。

## Consequences

- 三层 ACK 必须明确定义且不可混用：传输送达、接收方送达、已读。未读数只由 Read Marker 驱动（见 CONTEXT.md）。
- 每个 Conversation 需要维护 `next_seq`，写入路径必须原子分配 seq，否则排序权威失效。
- 客户端必须持久化"最后消费位置"，否则断线重连退化为全量重新同步。
- 这决定了服务端需要一份**短期回放缓冲**（处于连接层，非持久存储层），以及一份**按会话的持久消息流**。
