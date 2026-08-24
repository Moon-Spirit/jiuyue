# JiuYue M1 交付报告（垂直切片）

> 日期：2026-08-24 · 范围：docs/tickets/m1/ 全部 10 张任务票 · 结论：**M1 通过**

## 一、验收对照（用户故事 × 证据）

| #   | 用户故事                         | 结果 | 证据                                                                 |
| --- | -------------------------------- | ---- | -------------------------------------------------------------------- |
| 1   | 邮箱+验证码注册                  | ✅   | auth_flow.rs 9 测试绿；e2e-ws 转录 register 201                      |
| 2   | 手机号+短信注册（开发码 000000） | ✅   | auth_flow.rs phone 用例；shots.mjs 种子成功                          |
| 3   | 用户名+密码登录任意设备          | ✅   | login→refresh→ticket 全链路测试；浏览器实测                          |
| 4   | 搜索用户名直接发起会话           | ✅   | POST /api/conversations 集成测试 + UI 新建会话                       |
| 5   | 发送重试+服务端去重              | ✅   | chat_flow.rs 幂等用例（N 次重发=1 行）；m1-e2e dedupe 断言           |
| 6   | 离线消息完整补齐、顺序正确       | ✅   | sync_flow.rs 断线补齐；m1-e2e sync.fills_exact_gap [2,3]             |
| 7   | 多端历史一致                     | ✅   | GET /api/conversations 引导+全量同步（本轮修复，见下）               |
| 8   | 已送达状态                       | ✅   | msg.ack→✓✓已送达（截图 desktop-1280x800.png）                        |
| 15  | 暗色主题                         | ✅   | mobile-dark-390x844.png                                              |
| 16  | 中英切换                         | ✅   | i18n completeness 测试；LanguageToggle                               |
| 17  | 移动单栏体验                     | ✅   | mobile-390x844.png 布局证据                                          |
| 18  | compose 一键起全套               | ✅*  | deploy/docker-compose.yml（本机无 Docker 走 local-dev.ps1 等价路径） |

## 二、双接缝证据

- **领域/协议/后端**：`cargo test --workspace` = **67 passed**（domain 16 / protocol 7+golden 2 / server 单测 13 / auth 10 / chat 11 / sync 8）；`cargo clippy --workspace -- -D warnings` 干净。
- **前端**：`pnpm typecheck` exit 0；vitest **59/59**（ws store 状态机/auth store/i18n 完整性/组件冒烟）。
- **真实网络端到端**：`node scripts/qa/m1-e2e.mjs` → **7/7 断言 PASS**，完整 JSONL 转录：`docs/reports/m1/evidence-ws-transcript.jsonl`。
- **视觉证据**：`docs/reports/m1/screenshots/` 四视口 PNG（desktop 三栏含靛蓝右对齐气泡+已送达回执 / tablet 双栏 / mobile 单栏 / mobile 暗色）。
- **Tauri 冒烟**：`docs/reports/m1/tauri-smoke.txt` — release exe 存活 12s、窗口标题 JiuYue。

## 三、本轮视觉 QA 发现并修复的缺陷

1. **新设备会话列表为空**（违反故事7）：缺少会话发现端点。修复：新增 `GET /api/conversations`（TDD RED→GREEN）+ 前端引导合并 + 本地缓存从 seq 0 全量同步。
2. **自己发的消息渲染成对方样式**：登录响应缺 profile 导致 `userId=""`。修复：登录响应补 `user_id/username`（TDD）+ 前端 AuthUser 统一为 string 类型链。
3. 未读数双重计数（引导近似值与逐条累加叠加）→ 改为设备本地语义。
4. 侧栏"退出登录"文字换行 → `whitespace-nowrap`。

## 四、已知限制与二期建议

- 会话 ID 采用 BIGINT identity（协议 crate 冻结为 i64 wire id 所迫），与用户/消息 UUID 并存；二期协议 v2 可统一。
- last_delivered/last_read 游标为成员级而非设备级——多设备已读状态是共享的；M2 引入 read.update 时建议拆分设备游标表。
- 生产短信/APNs/FCM 为 Mock 通道，接口签名不变，凭证到位即插即用。
- Docker Desktop 本机未装：compose 文件为标准交付物，CI/其他机器可验证；本机 QA 走等价原生栈（PG16 服务 + tporadowski Redis）。
- Tauri 生产构建的前端直连后端绝对地址（无 vite 代理）在 QA 阶段以环境配置注入，M4 双区部署时统一收敛为 region-aware 配置。

## 五、teardown 凭证

QA 结束时停止：jiuyue-server.exe 后端进程、vite dev server、playwright chromium 临时上下文。持久服务保留：PostgreSQL 16（系统服务）、redis-server（C:\tools\redis，PID 文件随附）。测试数据（qa__/vis__/dbg_* 用户与会话）留存于 jiuyue_dev 库供复核，可用 `TRUNCATE users CASCADE` 清空。
