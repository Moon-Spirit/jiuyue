# 06 — 投递与离线同步：连接注册表、实时扇出、游标同步

**What to build:** 服务端连接注册表 `DashMap<UserId, HashMap<DeviceId, ConnHandle>>`（每连接有界 mpsc(512) + try_send 背压 + 幽灵连接驱逐）；心跳 ping 30s/超时 60s；msg.new 经注册表直投（单实例），Redis pub/sub `conv:{id}` 唤醒钩子同位预留多实例；每设备每会话游标 last_delivered_seq/last_read_seq 持久化；sync.req 按 `seq > cursor` 补发。集成测试跑本机测试栈，覆盖断线补齐与自回声。

**Blocked by:** 05-chat-send-path

**Status:** ready-for-agent

- [ ] 断线场景：B 发送中途断开 → A 继续发 → B 重连发 sync.req → 补齐全部缺口且 seq 连续
- [ ] 自回声：A 设备1 发的消息出现在 A 设备2
- [ ] 背压：慢客户端塞满 mpsc 后服务不阻塞、不断言丢帧（DB 游标兜底）
- [ ] 心跳：模拟 60s 无 pong 的连接被回收，注册表无泄漏条目
- [ ] Redis pub/sub 发布被调用（可用嵌入式断言/mock redis 验证钩子）
