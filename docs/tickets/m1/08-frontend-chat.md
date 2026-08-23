# 08 — 前端聊天：会话列表、消息流、实时 store

**What to build:** Pinia 会话/消息 store 接 WS（ticket 升级、指数退避重连、重连后按游标自动 sync）；会话列表（最后消息预览、未读计数、按最近排序）；聊天窗消息流（气泡、时间分组、发送中/失败重发状态、duplicate ACK 去重合并）；乐观发送 + client_msg_id 生成。已读/撤回 UI 位预留（M2 填充逻辑）。组件冒烟测试覆盖 store 状态迁移。

**Blocked by:** 06-delivery-sync, 07-frontend-shell

**Status:** ready-for-agent

- [ ] 双浏览器上下文集成取证：A 发 → B 实时收；B 断网期间 A 发 3 条 → B 恢复后自动补齐且顺序正确
- [ ] 断网重连期间用户发送的消息进入 pending 态并在恢复后成功去重落位（不产生双条）
- [ ] 未读计数在 B 阅读会话后归零（本地视角）
- [ ] Vitest：store 处理 msg.new/sync.res/msg.ack/error 帧的状态迁移测试绿
