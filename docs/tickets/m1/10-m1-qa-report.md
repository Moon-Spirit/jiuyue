# 10 — M1 集成 QA 与交付报告

**What to build:** 全链路手动 QA 执行与证据归档：compose 起服 → HTTP API curl 取证（注册/登录/建会话）→ websocat 双客户端 WS 会话记录（发送/ACK/接收/sync）→ Playwright 浏览器双实例互发截图（桌面三栏+移动视口各一）→ Windows 桌面构建物与浏览器实例互发取证。产出 `docs/reports/m1-qa.md` 交付报告：场景×结果×证据路径对照表 + 已知限制。

**Blocked by:** 09-tauri-desktop-shell

**Status:** ready-for-agent

- [ ] 每个用户故事（规格 18 条中 M1 范围内的 1–8、15–18 条）对应一行验收记录
- [ ] 双证据齐备：测试 GREEN 记录 + 真实表面工件（curl/websocat 日志、截图）路径全部有效可打开
- [ ] 清单化 teardown：QA 用临时进程/容器/数据卷清理完毕并留凭证
- [ ] 交付报告含「已知限制与二期建议」小节
