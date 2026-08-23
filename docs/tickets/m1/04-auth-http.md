# 04 — 认证与设备：双通道注册、登录、刷新令牌、WS ticket

**What to build:** HTTP API：邮箱验证码注册（开发期 SMTP Mock，验证码日志输出）+ 手机号短信注册（开发期固定验证码 `000000`，供应商适配器 trait 化留生产位）→ argon2id 密码设置 → 用户名密码登录签发 access(15m)/refresh(30d) JWT → 刷新端点 → 一次性 WS ticket 端点（5 分钟有效）。用户名全局唯一，凭证表支持同身份多绑定。testcontainers PG 集成测试覆盖全流程。

**Blocked by:** 01-monorepo-scaffold

**Status:** ready-for-agent

- [ ] curl 全链路取证脚本可跑通：注册(邮箱)→设密→登录→refresh→ticket
- [ ] 手机号通道用 `000000` 通过；错误验证码 401 且不泄露账号存在性
- [ ] 重复用户名/重复凭证绑定返回 409；弱密码被拒
- [ ] ticket 单次有效、过期失效（重放测试）
- [ ] 迁移可重复执行（sqlx migrate 幂等），测试库隔离
