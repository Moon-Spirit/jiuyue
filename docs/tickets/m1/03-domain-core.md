# 03 — 领域核心：seq 分配器 trait、消息状态机、撤回窗口策略（纯函数）

**What to build:** `jiuyue-domain` 纯逻辑层：`SeqAllocator` trait（接口 + 内存实现供测试；DB 实现在 05 接入）、`MessageStatus` 状态机（sending→sent→delivered→read→recalled，非法迁移拒绝）、recall 窗口策略（发送者 2 分钟内可撤；接收端撤回仅删内容留 tombstone）。TDD：先写失败测试。

**Blocked by:** 01-monorepo-scaffold

**Status:** ready-for-agent

- [ ] 状态机全迁移矩阵表驱动测试：合法迁移绿、非法迁移返回明确错误
- [ ] recall 策略边界测试：窗口内/外、非发送者、已撤再撤
- [ ] SeqAllocator trait 单测：单调递增、并发分配无重复（内存实现）
- [ ] 零 IO 依赖（无 sqlx/redis 引用），编译期强制
