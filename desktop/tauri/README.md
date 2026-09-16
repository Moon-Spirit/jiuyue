# jiuyue desktop shell — Tauri v2（Windows / macOS）

`desktop-shell` 接口的一个实现（另一个是 #46 的 Electron 壳）。前端复用的是同一份
`frontend/` 构建产物，壳之间不共享代码。

## 结构

```
desktop/tauri/
├── package.json              # @tauri-apps/cli（devDependency）
├── src-tauri/
│   ├── Cargo.toml
│   ├── build.rs
│   ├── tauri.conf.json       # 窗口 / 打包配置；frontendDist 指向 frontend/dist
│   ├── capabilities/
│   │   ├── default.json      # 主窗口：core:default + notification:default
│   │   └── call-window.json  # 通话窗口：只有 core:default（不能发通知）
│   ├── icons/                # 由 `tauri icon` 生成，已提交
│   ├── app-icon.png          # 图标源文件（1024×1024）
│   └── src/
│       ├── main.rs           # 只负责启动
│       ├── lib.rs            # Builder：插件、命令、托盘、窗口事件
│       ├── tray.rs           # 托盘：关闭→最小化、未读、退出
│       ├── call_window.rs    # 通话窗口：独立 OS 窗口 + 打开事件
│       └── commands.rs       # 接口对应的三个命令（薄封装）
```

## 行为

**关闭窗口 = 最小化到托盘，不退出。** `lib.rs` 拦截主窗口的 `CloseRequested`，
`prevent_close()` + `hide()`：WebView 不销毁，实时 WebSocket 继续跑，后台通知才可能送达。
退出是一条**显式**路径 —— 托盘菜单里的「退出」。

**托盘显示未读。** 前端把未读总数发给 `set_unread`，Rust 同时更新托盘 tooltip 与菜单里的
未读行（`0` 显示「暂无未读」，而不是消失）。清空未读不清空提示：状态本身是信息。

**托盘左键 = 唤回主窗口**，右键 = 菜单（平台惯例）。

**通话在独立窗口。** `open_call_window` 建一个 `WebviewWindow`（960×640，可缩放，居中），
label 形如 `call-<conversationId>-<audio|video>`；已存在则聚焦。窗口**加载完成后**向它自己的
文档发 `shell://call-open` 事件，通话界面据此知道要加入哪个会话（前端通过
`onCallWindowOpen` 订阅）。label 会被清洗，只保留字母数字、`-`、`_`；`close_call_window`
拒绝关闭任何非 `call-` 开头的窗口。

**原生通知**由 `tauri-plugin-notification` 负责（前端 `notify()`）；权限在 JS 侧查询与申请。
「聚焦抑制」在前端策略层（`frontend/src/shell/notifications.ts`）：窗口有焦点**且**正在看该会话
才不弹。

## 构建

```powershell
pnpm --dir desktop/tauri install
pnpm --dir desktop/tauri build        # = tauri build，自动先跑前端构建
```

`beforeBuildCommand` / `beforeDevCommand` 的工作目录是**应用目录**（即 `desktop/tauri/`），
所以路径写的是 `../../frontend`。

## 图标

`icons/` 由 CLI 从 `app-icon.png` 生成；需要换图标时：

```powershell
pnpm --dir desktop/tauri exec tauri icon src-tauri/app-icon.png
```

生成后删掉 `icons/android`、`icons/ios` —— 本壳只发布 Windows/macOS。

## 已知未做

- **CSP 仍是 `null`**（Tauri 默认）。收紧 CSP 前需要能在真实 WebView 里回归验证，否则会静默
  断掉样式或 WebSocket。
- **macOS 未构建/未验证**（本机是 Windows）；`icon.icns` 已生成，`Info.plist` 的
  `NSCameraUsageDescription` / `NSUsageMicrophoneUsageDescription` 属于 #30。
- **WebView2 的媒体权限回调未显式处理**（ADR-0007 点名过）—— 属于 #30。
- 通知的**点击路由**在 Windows 上用「窗口重新获得焦点 + 最近一次通知的会话」推断，
  不是真正的事件回调；见 `frontend/src/shell/tauri.ts` 的注释。
