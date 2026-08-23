# 02 — WS 协议 crate：v1 帧模型与 serde 往返

**What to build:** `jiuyue-protocol` crate 定义全部 v1 帧类型：信封 `{v:1, t, d}`；类型集 = auth.ticket / msg.send / msg.ack / msg.new / sync.req / sync.res / error（M1 集），预留 read.update / typing / recall / e2ee.* / signal.* 命名空间。附带手写 TS 镜像类型与一致性测试样例 JSON（golden files）。前端可 import 同构帧定义。

**Blocked by:** 01-monorepo-scaffold

**Status:** ready-for-agent

- [ ] 每个帧类型 serde 往返测试绿（Rust↔JSON golden file 双向）
- [ ] 未知 `t` 反序列化为带错误码的 error 帧，不 panic（前向兼容测试）
- [ ] TS 侧类型与 golden JSON 通过 vitest 校验一致
- [ ] crate 文档列出全类型表 + 一条完整会话示例帧流
