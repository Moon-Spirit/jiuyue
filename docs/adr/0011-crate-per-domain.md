# 后端按领域分 crate，契约类型集中在单一契约 crate

Status: accepted

后端是一个 Cargo workspace，**每个限界上下文一个 crate**（`jiuyue-contract`、`jiuyue-store`、`jiuyue-realtime`、`jiuyue-auth`，后续继续增加）。跨 crate 只能通过对方的公开 API 通信；**每张数据表只归一个 crate 所有**，任何 crate 都不得直接读别的 crate 的表。所有对外的线上类型（WebSocket 信封与 REST 请求/响应）**只存在于 `jiuyue-contract`**，前端的 TypeScript 由它生成。

工作区成员写成通配 `crates/*`，因此新增一个 crate 不需要改动工作区根清单 —— 这条主要是为了并行开发时文件所有权清晰。

## 理由

1. **模块化是本项目的硬约束**（见 `AGENTS.md`）。用目录分模块只是约定，用 crate 分模块是**编译器强制的边界**：跨边界的耦合会直接编译失败，而不是在 code review 时才被发现。
2. **可独立测试与替换**：`jiuyue-store` 的迁移测试跑真实 PostgreSQL、`jiuyue-contract` 的导出测试只依赖类型系统 —— 互不牵连。
3. **契约只有一个真源**：如果允许各 crate 各自定义线上类型，前后端契约就会漂移；把它集中到一个 crate 并从它生成 TS，漂移会变成编译错误或 CI 失败。
4. **并行开发不打架**：多个 agent/开发者同时干活时，"谁拥有哪些文件"是明确的，而不是靠默契。

## Considered Options

- **单一 `server` crate + 内部模块** — 否决：模块边界只是建议，可以随手 `use` 到内部细节；且多人并行时所有人都在改同一批文件。
- **按层分 crate（api / service / repository）** — 否决：层不是领域。按层切会把一个业务能力撕成三处，改动一个功能要动三个 crate，而且极易产生循环依赖。领域边界比分层边界更稳定。

## Consequences

- crate 数量会增长，每个都要一份 manifest。用 `crates/*` 通配 + 根 `[workspace.dependencies]` 统一版本，把重复压到最低。
- 依赖方向必须保持单向：`contract` ← `store` ← (`auth`, `realtime`) ← `server`。出现环就说明领域边界划错了，应当先改设计而不是绕过。
- **跨领域的读操作需要显式接口**：如果 A 领域需要 B 领域的数据，B 必须暴露一个方法，而不是让 A 直接查表。这条会带来少量样板代码，是刻意付出的代价。
- 秘密聊天的明文隔离依赖这条规则：审核模块拿不到秘密聊天的内容，是因为**结构上无法访问**，而不是因为查询条件里写了过滤（见 `docs/spec/0001-product-spec.md` 管理后台部分）。

## 相关的既定约定

首个迁移（`backend/migrations/20260917120000_users.sql`）在文件头写明了 schema 约定，后续迁移必须遵守：ULID 主键存 `CHAR(26)` 且用 CHECK 钉住形状、username/email 统一小写存储、时间戳一律 `timestamptz` 且由 PostgreSQL 默认生成。
