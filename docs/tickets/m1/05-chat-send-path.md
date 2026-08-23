# 05 — 单聊发送路径：会话创建、persist-then-ack、幂等去重、seq 落库

**What to build:** POST /conversations（指定对端用户名，create-or-get 直聊会话）；WS 用 ticket 升级鉴权；msg.send → 同事务 `UPDATE conversations SET last_seq=last_seq+1 RETURNING` 分配 seq 并 INSERT 消息 → 回 ACK(含 server_id/seq)；`UNIQUE(conversation_id, client_msg_id)` 冲突时返回 duplicate ACK 携既有 id/seq。body 列 AES-256-GCM 应用层加密落库（主密钥 env 注入，key_id 列留位）。集成测试跑本机测试栈（`jiuyue_test` 库 + Redis，env 驱动连接串，每运行重置 schema）：A/B 双模拟客户端全环。

**Blocked by:** 02-protocol-frames, 04-auth-http

**Status:** ready-for-agent

- [ ] 集成测试：A 注册登录建连 → 建会话 → 发消息 → 收 ACK(server_id+seq) → B 收 msg.new
- [ ] 幂等测试：同 client_msg_id 重发 N 次，库中仅 1 行，每次均得相同 (id, seq)
- [ ] 断言 body 密文落库（DB 直查非明文）、读取侧解密还原
- [ ] 未认证 WS 连接被拒；他人会话消息不可见
