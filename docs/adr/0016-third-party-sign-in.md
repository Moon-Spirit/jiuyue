# 第三方登录：provider 身份是账号的凭据，不是认领账号的方式

Status: accepted

第三方登录（GitHub / Google）看似只是"多一个登录按钮"，真正的决策只有一条：**当 provider 声称的邮箱与既有账号一致时，怎么办**。本 ADR 记录这条规则、`state`/PKCE 的处理、以及首次登录用户的"受限会话"，因为这三件事决定了这个功能是安全还是账号接管漏洞。

## 决定

### 1. provider 身份 = `(provider, subject)`，永不按邮箱或用户名匹配

- `oauth_identities` 以 `(provider, subject)` 唯一索引定位账号。`subject` 是 provider 自己的**不可变账号 ID**（OpenID Connect 的 `sub`；GitHub 是数字 `id`，不是可改可被他人重新注册的 `login`）。
- 邮箱只作为**展示信息**存下来（`oauth_identities.email` / `email_verified`），**从不作为匹配键**。provider 可以改邮箱、可以不验证邮箱、两个 provider 可以对两个不同的人报同一个地址——以邮箱为键就是把"谁能声称这个地址"等同于"谁拥有这个账号"。
- 一个账号可持有多个 provider 身份；一个 provider 身份只属于一个账号（唯一索引强制，`ON CONFLICT DO NOTHING` 保证并发下也是事实）。

### 2. 邮箱撞车 = 拒绝，不是自动绑定，也不是再建一个

provider 报来的邮箱已经属于某个账号时，三条路只有一条安全：

| 做法                                             | 后果                                                                                                                           |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| 静默把 provider 绑到既有账号                     | **账号接管漏洞**。任何能让 provider 断言某地址的人（provider 不验证邮箱、provider 账号被盗、租户配置错误）都能走进别人的账号。 |
| 静默再建一个同邮箱账号                           | 重复账号乱局，唯一约束本来也会拒绝。                                                                                           |
| **拒绝，并让用户用原来的方式登录后在设置里绑定** | 正确。                                                                                                                         |

代码位置：`AuthService`/`OAuthService::callback` → `create_account`，命中即返回 `AuthError::AccountExists` → HTTP `409 OAUTH_ACCOUNT_EXISTS`。这条判断在**任何写操作之前**执行（`email_is_taken`），竞态漏网由 `users_email_key` 兜住，同样是拒绝。

新增邮箱经 provider 进来则**建新账号**（provider 对自己身份的断言是权威），地址在 provider 声明已验证时记为已验证。

### 3. `state` 单次使用、10 分钟过期；PKCE 用 `S256`，verifier 由 state 派生

- `state` 是 256 位随机值，只存 SHA-256 摘要（`oauth_states.state_hash`），兑换是一次原子 UPDATE（`consumed_at IS NULL AND expires_at > now()`），并在同一条语句里把 verifier 摘要清空。重放 → `OAUTH_STATE_INVALID`，且**在调用 provider 之前**就拒绝。
- **不复用 `account_tokens`**：它的 `user_id` 是 `NOT NULL REFERENCES users`，而 state 在用户还不知道是谁时就已签发（首次登录）；复用会把一条本来干净的既有表改成一张含义混杂的表。`oauth_states` 是同一套纪律（只存摘要、数据库时钟判过期、原子消费）换一张表。
- **PKCE verifier 不额外存**：它是 `state` 的存储摘要（64 位十六进制，落在 RFC 7636 的 43–128 区间内）。`code_challenge = BASE64URL(SHA256(verifier))` 随授权请求发出；回调时从已存的那一列取出 verifier 去换 token。攻击者从授权 URL 只能看到 `state` 与 challenge，两者都推不出 verifier（SHA-256 原像），PKCE 的保证完整。少一个需要持久化的秘密。
- `redirect_uri` 来自配置（`OAUTH_CALLBACK_BASE_URL`，默认 `APP_BASE_URL`），**绝不从请求头取**；返回路径 `return_to` 必须是站内相对路径（`^/[^/]`），否则 422——这是开放重定向的另一半。

### 4. 首次登录用户没有用户名，这是一个真实状态：**受限会话**

- 该用户的账号已存在（provider 说了他是谁），但**未选择用户名前不可用**。表示方式：一枚 **256 位不透明 token**（`oauth_pending_sessions.token_hash` 只存摘要），**不是 JWT**。
- 因此它在**所有受保护端点被天然拒绝**：每个端点的凭据是 `Authorization: Bearer <JWT>`，不透明 token 不是 JWT，`authenticate` 的第一道签名校验就失败。这是"结构上不可能混淆"，不是"记得检查一个 flag"。
- 唯一接受它的是 `POST /auth/oauth/complete`：它在**同一条语句**里写下用户名并消费 token（`claim_username`）。用户名撞车（唯一索引）**什么都没写**，所以同一枚 token 可以重试——这就是"这个名字被占了，换一个"能实现的原因。
- 用户名规则与注册完全一致（`validate_username_choice` 复用 `check_username`）。
- **中断可恢复**：判断"是否还在 onboarding"问的是 `oauth_pending_sessions` 里有没有未消费的行，而不是 `password_hash IS NULL`——**已完成的 OAuth 账号同样没有密码**，用后者会把回访用户永远打回用户名步骤。再次用同一 provider 登录时，旧受限 token 作废、为同一账号新发一枚，用户不会被一枚丢失的 token 封死。
- 账号创建时若 provider 未提供邮箱，用 `.invalid` TLD 的占位地址（RFC 2606，永不可能是真实邮箱），以满足 `NOT NULL UNIQUE`；这是可见的占位符，不是"将来可能真发信的地址"。

### 5. 不存 provider 的 access / refresh token

交换拿到 token → 读取身份 → 立刻丢弃。这个功能之后不会以用户名义再调用 provider，存下来只是一个"赚不到任何东西、却会泄露"的第二份凭据。

### 6. 配置：凭据成对，缺一即"未配置"

- `GITHUB_CLIENT_ID`/`GITHUB_CLIENT_SECRET`、`GOOGLE_CLIENT_ID`/`GOOGLE_CLIENT_SECRET`。client id 或 secret 缺一 → 该 provider **不出现在 `GET /auth/oauth/providers`**（登录页就没有这个按钮），直接请求它返回 `503 OAUTH_NOT_CONFIGURED`——不是信任客户端只点它看到的东西。
- 端点与 scope **是代码不是配置**（`jiuyue_auth::oauth::provider` 的常量）：它们是协议的一部分，可配置只会让部署悄悄跑在一个错的端点上。
- 一个 provider 都没配时，实例不构建 OAuth 服务，`providers` 返回 `[]`，登录页第三方区块整体不渲染。**secret 绝不下发前端**；授权 URL 只带公开的 client id。

### 7. provider 的错误是自己的词汇，绝不回传

provider 的失败（`bad_verification_code`、token 端点 5xx、网络不通）统一映射为 `502 OAUTH_PROVIDER_ERROR`，原始文本只进日志。它的 body 可能回显 secret，且对我们的用户毫无意义。

## Considered Options

- **按邮箱自动绑定既有账号** — 否决：账号接管漏洞，本 ADR 存在的主要理由。
- **provider 邮箱撞车时再建一个同邮箱账号** — 否决：唯一约束会拒绝，且语义上就是错的。
- **以邮箱或用户名匹配 provider 身份** — 否决：二者都可变、可被他人重新占用。
- **复用 `account_tokens` 存 state** — 否决：其 `user_id` 为 NOT NULL FK，而 state 在身份未知时签发；改它等于污染一张语义清晰的表。
- **单独持久化 PKCE verifier（或加密存储）** — 否决：verifier 可由 `state` 摘要确定性派生，多存一份只是多一个秘密。
- **受限会话用带 flag 的 JWT** — 否决：flag 是"记得检查"的约定，忘了检查一次就是漏洞；不透明 token 让"到处被拒"成为默认。
- **用 `password_hash IS NULL` 判断未完成 onboarding** — 否决：已完成的 OAuth 账号也没有密码，会把回访用户永远打回用户名步骤（这正是开发中被测试抓住的一个 bug）。
- **把 provider 端点/scope 做成环境变量** — 否决：它们是协议，不是部署参数。
- **存 provider token 以备"以后可能要用"** — 否决：YAGNI + 多一份可泄露凭据。

## Consequences

- **没有网络就无法真正打通一次真 provider**。适配器在 `OAuthClient` 这一条缝上被 fixture 驱动，端到端（真 router / 真服务 / 真库 / 真 state 与 PKCE）全部可测；**但请求形状是否与 GitHub / Google 线上契约逐字段一致，本环境未能验证**。上线前必须用真凭据手工走通两个 provider。
- 遗留的 `oauth_states` / 已消费的 `oauth_pending_sessions` 行不会自动清理（量极小）；将来按运维脚本清理即可。
- 设置页"绑定 provider"的入口是后台接口 `POST /auth/oauth/link`（凭 bearer token 取账号，body 无法指定别人），前端 UI 尚未提供——这是刻意留下的下一步，不是遗漏。
- 新增依赖 `reqwest`（rustls + webpki roots）：服务端已有 rustls+ring，webpki 的信任根编进二进制，2C2G 香港机器与 CI 行为一致，不依赖系统 CA 商店。
