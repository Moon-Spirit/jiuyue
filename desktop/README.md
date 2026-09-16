# 桌面端（双壳）

桌面端做**两个可互换的壳，共用一份前端**（[ADR-0008](../docs/adr/0008-dual-desktop-shell.md)）：

| 目录                | 平台          | 引擎                           | 状态                       |
| ------------------- | ------------- | ------------------------------ | -------------------------- |
| `desktop/tauri/`    | Windows/macOS | Tauri v2（WebView2/WKWebView） | 本仓库已实现（ticket #45） |
| `desktop/electron/` | Linux         | Electron（Chromium）           | 未实现（ticket #46）       |

两者之间只隔一层接口：`frontend/src/shell/`。**前端组件、视图、store、router 都不得直接调用任一端专有 API** —— 这条规则由 `frontend/src/shell/guard.spec.ts` 机械校验，不是约定。

---

## 1. 接口：`frontend/src/shell/`

| 文件               | 职责                                                                        |
| ------------------ | --------------------------------------------------------------------------- |
| `types.ts`         | `DesktopShell` 能力面（唯一真源）                                           |
| `web.ts`           | 浏览器实现（弹窗通话、Web Notification、Badging API）                       |
| `tauri.ts`         | Tauri v2 实现（Rust 命令 + 官方通知插件）                                   |
| `index.ts`         | 接缝：`desktopShell()` / `installDesktopShell()` / `installDetectedShell()` |
| `notifications.ts` | 通知策略：聚焦抑制、免打扰、自己发的消息不通知                              |
| `unread.ts`        | 未读数 → 托盘投影                                                           |
| `guard.ts`         | "禁止绕过接口" 的纯函数扫描器                                               |
| `fake-shell.ts`    | 测试用的全 spy 实现                                                         |

能力面（详见 `types.ts`）：

- **通话**：`startCall`、`openCallWindow`、`closeCallWindow`、`onCallWindowOpen`
  —— 通话**必须**是独立 OS 窗口（产品硬需求），接口只返回窗口句柄，不返回平台窗口对象。
  媒体本身（信令）是 ticket #30，不在这层。
- **屏幕共享**：`pickScreenShareSource`（返回 `null` = 交给平台自带选择器：浏览器/WebView2 走
  `getDisplayMedia`，Linux 走系统 portal）。
- **权限**：`requestMediaPermission`（只查询状态；真正的提示由 `getUserMedia` 在采集时触发）。
- **通知**：`notificationPermission`、`requestNotificationPermission`、`notify`、
  `isWindowFocused`、`onNotificationClick`。
- **托盘 / 角标**：`setUnread`。

## 2. 接缝要接到哪里（lead 的接线）

在 `frontend/src/main.ts` 里加这一行（外加一行 import）：

```ts
import { installDetectedShell } from "./shell";

void installDetectedShell();
```

`installDetectedShell()` 同步检测宿主（`__TAURI_INTERNALS__` 是否存在），在 Tauri 里**动态**
import `shell/tauri.ts` 并安装。浏览器里装的是 web 实现，因此 Tauri 代码不会进入浏览器包。

Electron 壳（#46）不需要改这里：它在 `mount` 之前直接 `installDesktopShell(createElectronShell())`。

通知与托盘未读数还需要读取应用状态，这两条线由 store 侧接（同样是每处一行）：

```ts
// 通知：把实时流里的 NewMessage 交给策略
const notify = createMessageNotifier(desktopShell(), {
  activeConversationId: () => chat.activeConversationId,
  windowFocused: () => desktopShell().isWindowFocused(),
  isMuted: (conversationId) => /* 免打扰状态 */ false,
  selfUserId: () => auth.user?.id ?? null,
  titleFor: (message) => /* 用户名或群名 + 用户名 */ message.sender_id,
});
realtime.onChatEvent((event) => {
  if (event.t === "NewMessage") void notify(event.d.message);
});

// 托盘未读
bindUnreadToTray(desktopShell(), () => totalUnread(chat.unreadCounts));
```

## 3. 构建与运行

```powershell
# 依赖（Tauri CLI 是 devDependency，不 vendor 二进制）
pnpm --dir desktop/tauri install

# 开发（Tauri CLI 自己起 frontend dev server）
pnpm --dir desktop/tauri dev

# 打包：先构建前端，再编译 Rust，再出安装包
pnpm --dir desktop/tauri build

# 只验 Rust 侧
pnpm --dir desktop/tauri run check     # cargo check
pnpm --dir desktop/tauri run test      # cargo test
```

产物：`desktop/tauri/src-tauri/target/release/bundle/`（Windows：`.msi` + `-setup.exe`）。

## 4. 规则怎么被机械保证

`guard.spec.ts` 用 Vite 的 `import.meta.glob` 把 `frontend/src/**` 全部读成文本，然后断言
`frontend/src/shell/` 之外没有任何文件：

- `import` / 动态 `import()` / `require()` 了 `@tauri-apps/*`、`tauri-plugin-*`、`electron`、`@electron/*`；
- 读写了 `__TAURI__` / `__TAURI_INTERNALS__`。

它随 `pnpm --dir frontend test` 一起跑。违规示例（实测失败信息）：

```
src/components/chat/__shell-guard-demo.vue → @tauri-apps/api/core:
expected [ { … } ] to deeply equal []
```
