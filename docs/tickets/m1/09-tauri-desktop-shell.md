# 09 — Tauri2 桌面壳：三平台配置与安全令牌存储

**What to build:** Tauri2 工程接入前端产物：窗口配置（尺寸/最小尺寸/标题 JiuYue）、应用图标、Win/macOS/Linux 构建配置；Rust command `secure_store`/`secure_load` 将 refresh token 写入应用数据目录（0600 权限，硬编码路径禁用）；CSP 收紧。产出 Windows 可执行构建物并与 compose 后端联调。

**Blocked by:** 08-frontend-chat

**Status:** ready-for-agent

- [ ] `pnpm tauri build` 产出 Windows exe/msi 且可启动
- [ ] 桌面实例注册新账号 → 与浏览器实例互发消息实时可达（截图+操作记录取证）
- [ ] refresh token 落盘文件权限 0600、内容非明文 JWT（加密或受限 DACL 方案说明）
- [ ] tauri.conf CSP 不含 `unsafe-eval`；devtools 仅 debug 构建启用
