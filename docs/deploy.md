# 生产部署：systemd + Caddy（无容器）

> 面向单台 2 vCPU / 2 GB 香港云服务器（Debian/Ubuntu）的首发部署。
> **不使用任何容器**（Docker / Compose / Podman / k8s），见 [`docs/adr/0010-no-containers.md`](./adr/0010-no-containers.md)。
> 内存与调参数据引自 [`docs/research/2026-09-16-vps-2gb-ops-research.md`](./research/2026-09-16-vps-2gb-ops-research.md)
> （该文里 Docker 专属的章节已随 ADR-0010 失效，PostgreSQL、swap、监控、备份章节仍然有效）。

---

## 1. 架构总览

```
                    ┌──────────────────────────── 2 vCPU / 2 GB ───────────────────────────┐
   Internet ──443──▶ Caddy (systemd, 自动 HTTPS)                                             │
                    │   /api/*  ──▶ 127.0.0.1:8080  jiuyue-server (systemd, 非特权用户)     │
                    │   /ws     ──▶ 127.0.0.1:8080  （WebSocket，独立 stream 调优）         │
                    │   /*      ──▶ /opt/jiuyue/current/dist  前端静态资源（同一软链接）     │
                    │                                              │                         │
                    │                          127.0.0.1:5432  PostgreSQL 16 (systemd) ◀────┘
                    └────────────────────────────────────────────────────────────────────────┘
```

- **进程生命周期**：systemd。日志进 journald（自带轮转与 `journalctl -u jiuyue`）。
- **资源限额**：systemd `MemoryMax` / `LimitNOFILE`（替代容器 cgroup 限额）。
- **制品**：CI 构建的 tar 包（二进制 + 前端静态资源）→ 服务器拉取 → 切换软链接 → 重启服务。
  **服务器上永不编译 Rust**（2C2G 跑 LTO release 会 OOM）。
- **回滚**：保留上一版 release 目录，软链接切回即可；迁移不回滚（见 §5）。

---

## 2. 目标机与前置条件

| 项     | 要求                                                                    |
| ------ | ----------------------------------------------------------------------- |
| 系统   | Debian 12/13 或 Ubuntu 24.04+，systemd，native 进程                     |
| 配置   | 2 vCPU / 2 GB RAM / 100 Mbps                                            |
| 域名   | 一个真实域名，A/AAAA 记录指向本机公网 IP（Caddy 用它签发证书）          |
| 端口   | 入站 80/tcp、443/tcp（HTTP/3 另需 443/udp）；**不要把 8080 暴露到公网** |
| 权限   | root（或 sudo）                                                         |
| 软件源 | 需要能访问 `dl.cloudsmith.io`（Caddy 官方 apt 源）与 Let's Encrypt      |
| 磁盘   | 建议 ≥ 40 GB；证书在 `/var/lib/caddy`，数据在 `/var/lib/jiuyue`         |

> 应用监听 `0.0.0.0:<PORT>`（`backend/crates/server/src/main.rs`），配置里没有绑定地址开关。
> 因此**必须**用防火墙挡住 8080，只让本机 Caddy 访问（见 §11）。
>
> **glibc 下限**：制品在 `ubuntu-24.04` 上构建，运行时 glibc ≥ 2.39，即 Ubuntu 24.04+ / Debian 13；
> Debian 12 未验证。原因与替代方案见 §4.1。

---

## 3. 一次性初始化（provision）

在服务器上，从本仓库的检出目录执行：

```bash
sudo bash deploy/provision.sh --domain chat.example.com --acme-email ops@example.com
```

> 用 `bash` 显式调用：仓库在 Windows 上开发，git 不记录执行位，克隆到 Linux 后脚本默认没有 `+x`。
> 想直接执行就先 `chmod +x deploy/*.sh`。

脚本是**幂等**的，可重复执行；它会：

1. 添加 Caddy 官方 apt 源，安装 `postgresql-16` / `postgresql-client-16` / `caddy` / `curl` / `openssl` / `util-linux`；
2. 创建系统用户 `jiuyue`（`nologin`）、数据目录 `/var/lib/jiuyue`（0750）、`/opt/jiuyue/releases`、`/etc/jiuyue`（0750 root:jiuyue）；
3. 若无 swap：创建 1 GB `/swapfile` 并写入 `/etc/fstab`；写 `vm.swappiness=10`、`net.core.somaxconn=1024`（`/etc/sysctl.d/90-jiuyue.conf`）；
4. 安装 PostgreSQL 调参 `20-jiuyue-tuning.conf` 并重启；
5. **首次**生成密钥：写 `/etc/jiuyue/jiuyue.env`（0640 root:jiuyue），随机 `JWT_SECRET`（`openssl rand -hex 32`）与数据库密码（`openssl rand -hex 24`），建 `jiuyue` 角色与 `jiuyue` 库（库 owner = 该角色，不需要 `CREATEDB`）；
6. 安装 systemd unit 与 drop-in（PG 内存上限、Caddy 环境文件），`daemon-reload`；
7. 写 `/etc/jiuyue/caddy.env`（域名/邮箱），安装 `/etc/caddy/Caddyfile`，**先用真实变量 `caddy validate` 再 reload**；
8. `enable` PostgreSQL / Caddy / jiuyue，重启 Caddy 触发 ACME 签发证书。

**已存在的 `/etc/jiuyue/jiuyue.env` 不会被覆盖**（密钥只生成一次）。若怀疑泄露，手动更换后 `systemctl restart jiuyue`。

初始化完不发布任何版本。下一步执行 §4；也可在 provision 时直接 `--artifact <path-or-url>` 串联发布。

---

## 4. 发布与部署（deploy）

### 4.1 制品接口（由 `.github/workflows/release.yml` 实现，ticket #44）

产物发布在 **GitHub Releases** 资产上（没有对象存储、没有镜像仓库，见 ADR-0010）。
打一个 `v*` 标签触发构建与发布；`push` 到 `main` 也会构建并冒烟，但没有标签就不发布。

| 项       | 约定                                                                                                      |
| -------- | --------------------------------------------------------------------------------------------------------- |
| 版本号   | `<version>` = git 标签去掉前导 `v`（标签 `v1.2.3` → `1.2.3`），同时进文件名与 `/health`                   |
| 文件名   | `jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz`                                                        |
| 内容     | `bin/jiuyue-server`（可执行）、`dist/`（`frontend/dist` 的内容）、`VERSION`（可追溯元数据）               |
| 校验     | 同名 `.sha256` 伴随文件（`sha256sum` 标准输出：`<64-hex>` + 两个空格 + 文件名；`deploy.sh` 取首行第一列） |
| 拉取     | HTTPS 下载，或本地路径                                                                                    |
| 构建基座 | `ubuntu-24.04` runner；后端 target `x86_64-unknown-linux-gnu`，release profile                            |

`VERSION` 文件（每行 `key=value`；`deploy.sh` 不解包读取它，仅供人/脚本排查）：

```
version=1.2.3
tag=v1.2.3
commit=<40-hex>
built_at=<ISO-8601 UTC>
target=x86_64-unknown-linux-gnu
```

**URL 模式**（`<tag>` 是标签，`<version>` 是去掉前导 `v` 的版本）：

```
https://github.com/Moon-Spirit/jiuyue/releases/download/<tag>/jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz
https://github.com/Moon-Spirit/jiuyue/releases/download/<tag>/jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256
```

`deploy.sh` 会自动去取 `<url>.sha256` 当校验，所以给 `--artifact` 一个 URL 就够：

```bash
sudo bash deploy/deploy.sh --artifact \
  https://github.com/Moon-Spirit/jiuyue/releases/download/v1.2.3/jiuyue-1.2.3-x86_64-unknown-linux-gnu.tar.gz
```

**可追溯性**：`version`、提交与构建时间可从三处取到 —— Release 页面与资产名里的 `<version>`、制品内的
`VERSION` 文件（`commit` / `built_at`）、以及运行中服务的 `GET /health`（`version` 字段，构建时经
`JIUYUE_BUILD_VERSION` 编译进二进制，所以标签构建报告的是标签版本而不是 crate 的 `0.1.0`）。

**构建基座与 glibc**：制品在 `ubuntu-24.04` runner 上链接，运行时依赖该基座的 glibc（2.39）。
因此 §2 的目标机矩阵里 Ubuntu 24.04+ 与 Debian 13 有把握，**Debian 12（glibc 2.36）未经验证**；
若实测缺符号，需要换用更低 glibc 的构建基座（例如 musl 静态构建）或自托管 runner。冒烟任务在**同一个**
runner 上执行，它证明不了更低 glibc 的兼容性。

发布前若要在本地复现打包（示例，**不要在服务器上编译**；CI 用同一套布局）：

```bash
cargo build --manifest-path backend/Cargo.toml --release --bin jiuyue-server
pnpm --dir frontend build
mkdir -p pkg/bin pkg/dist
install -m 0755 backend/target/release/jiuyue-server pkg/bin/jiuyue-server
cp -a frontend/dist/. pkg/dist/
printf 'version=%s\ntarget=x86_64-unknown-linux-gnu\n' "${VERSION}" > pkg/VERSION
tar -C pkg -czf "jiuyue-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" bin dist VERSION
sha256sum jiuyue-*.tar.gz > jiuyue-*.tar.gz.sha256
```

### 4.2 部署命令

```bash
# 本地文件，显式校验和
sudo bash deploy/deploy.sh --artifact ./jiuyue-0.1.0-x86_64-unknown-linux-gnu.tar.gz \
                           --sha256 <64-hex>

# 远端 URL（自动读取 <url>.sha256 作为校验）
sudo bash deploy/deploy.sh --artifact https://example.com/releases/jiuyue-0.1.0-x86_64-unknown-linux-gnu.tar.gz

# 额外检查公网域名（可选）
sudo bash deploy/deploy.sh --artifact <path> --public-url https://chat.example.com/api/health
```

`deploy.sh` 的选项：`--artifact`、`--sha256`、`--version`、`--health-url`、`--health-timeout`、`--public-url`、`--keep`、`--no-prune`、`--help`。

### 4.3 它做了什么

1. `flock` 串行化（防止并发发布）；
2. 下载/复制制品，**强制校验 sha256**——没有校验和就拒绝部署（`--sha256` 或同名 `.sha256`，二者皆无则报错退出）；
3. 解包到 `releases/<stamp>/`，校验 `bin/jiuyue-server` 与 `dist/index.html` 存在；
4. 原子切换软链接：`/opt/jiuyue/current -> releases/<stamp>`（`ln -sfn` + `mv -T`）；
5. `systemctl restart jiuyue`；
6. 轮询 `http://127.0.0.1:8080/health` 直到 `"status":"ok"`（默认最多 60 s）；
7. **健康检查失败自动回滚**：软链接指回上一版 → 重启 → 再检查；服务不会停留在半更新状态；
8. 成功后可传 `--public-url` 再校验一遍公网入口（失败只告警，不影响本机判定）；
9. 清理旧 release，保留最近 `--keep`（默认 3）个，**永不删除当前 active 版本**。

### 4.4 迁移（migration）

**迁移不是一个独立步骤**：迁移用 `sqlx::migrate!` 编译进二进制（`backend/crates/store/src/store.rs`），
服务启动、接受流量之前自动执行（`main.rs` 中 `Store::migrate()` 在 bind 之前）。因此：

- 新版本的迁移由**新二进制启动时**施加；
- 迁移失败 → 进程退出 → 健康检查失败 → `deploy.sh` 回滚到上一版二进制；
- **但数据库 schema 不会随二进制回滚**。所以迁移必须向前兼容一个版本（expand/contract）：
  先加列/加表（旧代码能容忍），下个版本再停用旧列，再下个版本才删。
  凡是"旧二进制无法运行"的破坏性迁移，都不要指望 `deploy.sh` 能救你。

---

## 5. 回滚

- **自动**：健康检查失败时 `deploy.sh` 自动切回上一版（§4.3 第 7 步）。
- **手动**（需要立刻回到上一版）：

```bash
ls -1 /opt/jiuyue/releases            # 找到上一个 release
sudo ln -sfn /opt/jiuyue/releases/<prev> /opt/jiuyue/current.new
sudo mv -Tf /opt/jiuyue/current.new /opt/jiuyue/current
sudo systemctl restart jiuyue
curl -fsS http://127.0.0.1:8080/health
```

- 若上一版也不健康：`journalctl -u jiuyue -n 200 --no-pager` 看原因，必要时改 `/etc/jiuyue/jiuyue.env`。
- **schema 不回滚**：见 §4.4。若必须回退破坏性迁移，需人工用 SQL 处理，且要有数据库备份（§10）。

---

## 6. 配置与密钥

运行时变量**只有一处**：`/etc/jiuyue/jiuyue.env`（systemd `EnvironmentFile=`），模板见
[`deploy/env/jiuyue.env.example`](../deploy/env/jiuyue.env.example)。真源是
`backend/crates/server/src/config.rs`，本表与其一致：

| 变量                     | 必填 | 默认      | 说明                                                             |
| ------------------------ | ---- | --------- | ---------------------------------------------------------------- |
| `DATABASE_URL`           | 是   | —         | PostgreSQL DSN；缺失/空白启动即失败                              |
| `JWT_SECRET`             | 是   | —         | HS256 签名密钥；**至少 32 字节**，过短在构造 token issuer 时被拒 |
| `PORT`                   | 否   | `8080`    | 绑定端口（`0.0.0.0`，必须防火墙挡住）                            |
| `RUST_LOG`               | 否   | `info`    | tracing filter                                                   |
| `ACCESS_TOKEN_TTL_SECS`  | 否   | `900`     | 访问令牌有效期（15 分钟）                                        |
| `REFRESH_TOKEN_TTL_SECS` | 否   | `2592000` | 刷新令牌有效期（30 天）                                          |

**密钥处理**：

- unit 文件里**内联零个密钥**；全部走 `EnvironmentFile`。
- `/etc/jiuyue/jiuyue.env` 权限 `0640 root:jiuyue`，仅 root 与 `jiuyue` 可读。
- 生成：`openssl rand -hex 32`（JWT）、`openssl rand -hex 24`（库密码）。`provision.sh` 首次自动生成，之后不覆盖。
- 仓库里只有**占位符**模板；真实值、真实域名、真实 IP 都不进 git。
- 运维备份该文件时同样要加密（§10）。
- 变量优先级高于工作目录里的 `.env`（`dotenvy` 不覆盖已存在的环境变量），所以 `WorkingDirectory` 里残留 `.env` 也盖不过它。

Caddy 的两个部署输入放在 `/etc/jiuyue/caddy.env`（`JIUYUE_DOMAIN`、`JIUYUE_ACME_EMAIL`，0640 root:caddy），由 systemd drop-in 注入。

---

## 7. HTTPS 与 Caddy

- **自动签发/续期**：Caddy 对 `{$JIUYUE_DOMAIN}` 走 ACME（Let's Encrypt），首次启动签发，到期前约 30 天自动续期。
  前提：DNS 已解析到本机、80/443 可达。证书存 `/var/lib/caddy`。
- **HTTP→HTTPS** 自动 301；HTTP/3（QUIC）默认开启，用 UDP 443。
- **admin API** 保持默认 loopback（`127.0.0.1:2019`），**绝不**对公网开放。
- **路由**：`/api/*` 去前缀转发到 `127.0.0.1:8080`（与 `frontend/vite.config.ts` 的 dev 代理一致）；
  `/ws` 原样转发并升级；其余走静态 `try_files {path} /index.html`（SPA 回退）。

### 7.1 WebSocket 与"重载会断开连接"

这是本项目明确的约束（`AGENTS.md`）：**Caddy 重载配置默认会强制关闭所有 WebSocket**。
Caddy 文档原文：连接在配置重载时被强制关闭，因为每个请求持有对旧配置的引用；可用
`stream_timeout` 与 `stream_close_delay` 定制。我们的设置（见 `deploy/Caddyfile`）：

| 指令                    | 值  | 作用                                                                                   |
| ----------------------- | --- | -------------------------------------------------------------------------------------- |
| `flush_interval -1`     | —   | 关闭响应缓冲，帧立即下发（实时消息必需）                                               |
| `stream_timeout 24h`    | 24h | 单条流最长存活 24h，之后强制关闭；客户端自动重连即可清理半死连接                       |
| `stream_close_delay 5m` | 5m  | `caddy reload` 时**不立即**关流，旧流最长保留 5 分钟，避免重连惊群；新连接立即用新配置 |

**必须写进心里的后果**：

- `systemctl reload caddy`（改证书/路由）不会立刻断流，5 分钟后旧流才关；客户端仍需具备指数退避重连。
- `systemctl restart caddy`、机器重启、网络抖动**仍会**断开全部连接——没有"零断线"。
- **应用发布也会断**：`deploy.sh` 重启的是后端进程，Caddy 到后端的 upstream 连接随之断开，与 Caddy 设置无关。
  因此前端必须有重连 + 重订阅逻辑（`AGENTS.md` 硬约束），服务端需容忍重连时的状态重建。
- 重连风暴是写放大风险：presence/last-seen 等易变状态不要每帧打 Postgres（research §(d)）。

---

## 8. 内存预算（2 GB 显式算术）

引自 research §Resource budget 与 §(a)/(c)/(d)，扣除容器运行时后重算（ADR-0010 省掉了 `dockerd`+`containerd` 约 100 MB）。

**核心三服务 + OS**（必须装下）：

| 组件                                  | systemd `MemoryMax`（上限） | 典型 RSS   | 依据                              |
| ------------------------------------- | --------------------------- | ---------- | --------------------------------- |
| OS + 内核 + systemd + journald + sshd | 不设上限（预留）            | 250–350 MB | research §(c) 宿主机预留          |
| PostgreSQL 16                         | 768 MB                      | 450–550 MB | research §Resource budget         |
| jiuyue-server                         | 320 MB                      | 20–80 MB   | research §Resource budget         |
| Caddy                                 | 128 MB                      | 15–35 MB   | research 给 96M，这里留 zstd 余量 |

- 上限之和：`768 + 320 + 128 = 1216 MB`
- 加 OS 预留 350 MB：`≈ 1566 MB`
- 相对物理 2048 MB 的**封顶剩余：≈ 482 MB**
- 典型 RSS 之和：`300 + 500 + 60 + 25 ≈ 885 MB`，**典型剩余 ≈ 1163 MB**

**后续可选项**（启用后再算）：

| 组件                          | 上限   | 典型      |
| ----------------------------- | ------ | --------- |
| coturn（1:1 通话中继）        | 192 MB | 64–130 MB |
| Beszel agent                  | 64 MB  | 10–25 MB  |
| Uptime Kuma（`-slim`+SQLite） | 192 MB | 60–100 MB |

启用全部可选后：上限之和 `≈ 2014 MB`，典型之和 `≈ 1090 MB`。
**上限互相叠加接近物理内存是刻意的**：`MemoryMax` 是"爆炸半径"，不是预留；真正必须装下的是"典型"列
（research §Resource budget 原文同此逻辑）。触顶时内核只杀该 cgroup 内的进程，而不是满机器挑一个大进程杀
（research §(c)：不给上限时，内核常杀掉数据库而放走泄漏者）。

**应用为何是 320 MB**：基线 RSS 约 12 MB，规划值 20–64 KB/空闲 WebSocket，
50–100 条并发连接合计仅 1–6 MB（research §(d)）。上限存在只是为了兜住分配器在负载后不归还的内存，
不是稳态目标。

**swap**：1 GB 文件 + `vm.swappiness=10`，是"报警缓冲"而非额外内存——换来发现并处理问题的时间。
不要在 2 GB 机器上开 zram（拿 CPU 换内存，而这台机器正缺 CPU，research §(c)）。

---

## 9. PostgreSQL 调参

安装到 `/etc/postgresql/16/main/conf.d/20-jiuyue-tuning.conf`，见
[`deploy/postgresql/jiuyue-tuning.conf`](../deploy/postgresql/jiuyue-tuning.conf)。要点（research §(a)）：

- `shared_buffers=384MB`（约 19%），**不是**独占服务器的 25%/512MB——本机还要跑应用与 Caddy；
- `effective_cache_size=1GB` 只是 planner 提示，不分配内存；
- `work_mem=4MB`：每个排序/哈希节点、每个并行 worker 各算一份，所以故意小；
- `max_connections=40`：连接槽在启动时就按上限分配数组，默认 100 是内存隐患；
- 应用侧 `sqlx` 连接池上限 8（`backend/crates/store/src/store.rs`），**不要**一连接一 Postgres；
- autovacuum 不禁用、不过度扩编（`max_workers=2`），高翻表按表单独收紧；
- **不需要 PgBouncer**：单进程 + 小池，且 sqlx 预处理语句是吞吐优势（research §(a)）。

---

## 10. 备份与灾难恢复

> 完整设计（制品布局、加密、保留策略、告警、恢复 runbook、未验证清单）见
> [`docs/backup.md`](./backup.md)。最近一次恢复演练的真实记录见
> [`docs/drills/2026-09-17-restore-drill.md`](./drills/2026-09-17-restore-drill.md)。
> 本节只留部署视角的要点。

research §(f) 的策略在无容器下**依然成立**，`docker compose exec pg_dump` 已替换为原生 `pg_dump`
（ADR-0010）。落地成三个脚本 + 一个 systemd timer，不再靠人肉敲命令：

| 命令                             | 作用                                                             |
| -------------------------------- | ---------------------------------------------------------------- |
| `deploy/backup.sh`               | dump + 归档文件存储 + **加密** + 上传 + 本地/远端分离保留 + 心跳 |
| `deploy/restore.sh`              | 从备份集恢复数据库与文件存储（sha256 不匹配即拒绝）              |
| `deploy/restore-drill.sh`        | 销毁 → 从远端恢复 → 逐字节比对；CI 每次推送真跑                  |
| `jiuyue-backup.timer`（systemd） | 每日 18:30 UTC（香港 02:30）触发 `jiuyue-backup.service`         |

要点（与 research §(f) 一致，容器相关内容作废）：

- **加密在上传之前**：`pg_dump -Fc | gzip -6 | gpg -c`，口令文件独立于备份；用 `rclone crypt` 时再叠一层；
- **上传用 `rclone copy`，绝不用 `sync`**——否则本地 7 天清理会把云端 30 天历史一起删掉；
  本地保留 7 天（`BACKUP_KEEP_LOCAL_DAYS`）与远端保留 30 天（`BACKUP_KEEP_REMOTE_DAYS`）是**两套独立策略**；
- **心跳监控**（Uptime Kuma push）：成功发 `status=up`，任何失败发 `status=down`；**心跳缺席本身就是告警**
  （备份没跑比备份失败更危险）；
- **文件存储必须一起备份**，不能只导数据库（ADR-0006，`/var/lib/jiuyue`）；
- **演练是交付物**：`.github/workflows/backup-restore.yml` 造数据 → 备份 → **销毁** → 恢复 → 比对，
  不通过即红；另外每季度在真实机器上演练一次。

密钥与口令的存放要求见 §6；`/etc/jiuyue/backup.env` 与 `/etc/jiuyue/backup_gpg_pass` 由
`deploy/provision.sh` 生成/安装，**口令必须同时存进密码管理器**——它是唯一能读回备份的东西。

---

## 11. 防火墙

应用绑定 `0.0.0.0:8080`，**必须**在主机防火墙封住 8080，只放 Caddy 需要的端口：

```bash
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp        # 建议配合仅密钥登录 + fail2ban
ufw allow 80/tcp        # ACME HTTP-01 与 HTTP->HTTPS 跳转
ufw allow 443/tcp
ufw allow 443/udp       # HTTP/3；不需要 QUIC 可省略
ufw enable
```

后续启用 coturn/LiveKit 时再单独放行 3478、5349 及其 UDP 中继段。无 Docker 也就没有
"发布端口绕过 UFW"的问题（research §(c) 该条已失效）。

---

## 12. 监控与告警

完整设计、告警目录、内存/日志算术与 runbook 见 [`docs/monitoring.md`](./monitoring.md)。
按 research §(e) 的轻量结论，**不要上 Prometheus+Grafana，也不要 Netdata**（后者 150-400 MB，是 2 GB 机器的三分之一）：

1. **`deploy/monitor/monitor.sh` + `jiuyue-monitor.timer`（每分钟）** —— 磁盘/内存阈值、关键服务停止、
   **`Restart=always` 的崩溃循环**，全部 POST 到 `ALERT_WEBHOOK_URL`；告警去重、恢复通知、重启记录到
   `/var/lib/jiuyue-monitor/restarts.log`。这是 CI 里可被**真实触发并捕获**的那条链路。
2. **Beszel（hub + agent，原生二进制）** —— 资源使用视图（CPU/内存/磁盘/网络历史），
   agent ~10-30 MB、hub ~30-50 MB；hub 只监听 `127.0.0.1:8090`，经 SSH 隧道访问。
   安装：`sudo bash deploy/monitor/install-beszel.sh --version <pinned>`。
3. **外部检查** —— 机器整体挂掉时，机内监控发不出任何东西，必须有一个**机器外**的第三方检查打公网 URL。
4. **日志上限**：journald `SystemMaxUse=200M`（后端与 Caddy 都进 journald）+ PostgreSQL 文件日志由
   `deploy/logrotate/jiuyue-postgresql` 以 `size 50M` × `rotate 5` 轮转（Debian 用 `pg_ctl -l` 重定向，
   `logging_collector=off`，PostgreSQL 自身的 `log_rotation_*` 不生效）。到顶时 journald 从旧到新 vacuum、
   logrotate 截断并删除最旧一份，`/` 占用长期稳定。

告警默认从宽到严起步：磁盘 80/90%、内存可用 15/8%、服务停止（连续 2 次）、崩溃循环（10 分钟内 ≥5 次）。
备份失败仍由 `backup.sh` 的心跳（`HEARTBEAT_URL`，可指向一个 Uptime Kuma push monitor，可在别处）通知。

---

## 13. 验证状态（诚实清单）

| 检查项                                                             | 在哪里验证                                     | 状态                          |
| ------------------------------------------------------------------ | ---------------------------------------------- | ----------------------------- |
| Caddyfile 语法 / 结构                                              | CI `caddy validate`                            | CI 每次推送                   |
| systemd unit 与 drop-in 语法                                       | CI `systemd-analyze verify`                    | CI 每次推送                   |
| 部署脚本语法 / lint                                                | CI `bash -n` + `shellcheck`                    | CI 每次推送                   |
| 无 CRLF、env 模板变量齐全                                          | CI                                             | CI 每次推送                   |
| 证书签发/续期、HTTP→HTTPS、真实域名路由                            | 需要真实服务器与域名                           | **未验证**                    |
| `MemoryMax`/`LimitNOFILE` 的实际生效                               | 需要真实服务器                                 | **未验证**                    |
| `deploy.sh` 端到端（下载→校验→切换→重启→健康→回滚）                | 需要真实服务器与 systemd                       | **未验证**                    |
| PostgreSQL 调参、swap 的实际表现                                   | 需要真实服务器负载                             | **未验证**                    |
| Caddy 重载时的 WebSocket 保留行为                                  | 需要真实浏览器与 live 连接                     | **未验证**                    |
| 制品打包内容（`bin/` + `dist/` + `VERSION`）                       | CI `Release` 工作流冒烟任务                    | CI 每次 push main / 标签      |
| 制品 `.sha256` 侧车能被 `sha256sum -c` 验证                        | CI `Release` 工作流冒烟任务                    | CI 每次 push main / 标签      |
| 制品能在 Linux 上真跑（真 PostgreSQL，`/health` = ok 且版本一致）  | CI `Release` 工作流冒烟任务                    | CI 每次 push main / 标签      |
| `deploy.sh` 端到端（假 systemd + 真二进制 + 真健康检查）           | CI `Release` 工作流冒烟任务                    | CI 每次 push main / 标签      |
| GitHub Release 发布（`v*` 标签 → tar.gz + 侧车可下载）             | 首次打 `v*` 标签时                             | **未验证**（仓库尚无标签）    |
| 备份/恢复脚本 lint、CRLF、systemd unit、env 模板                   | CI `Backup and restore` lint                   | CI 每次推送                   |
| **恢复演练**：备份 → 销毁 → 从远端副本恢复 → 逐表逐字节一致        | CI `Backup and restore` `restore-drill` + 本机 | **已验证**（见 drills 记录）  |
| 文件存储纳入备份（空文件、带空格文件名）                           | CI `restore-drill` / 本机演练                  | **已验证**                    |
| 备份失败会发出 `status=down` 心跳                                  | CI `alerting` / 本机捕获                       | **已验证**                    |
| 制品离开机器前确为密文（OpenPGP 包头 + 可解析）                    | CI `restore-drill` / 本机演练                  | **已验证**                    |
| `rclone` 上传真实对象存储、远端保留对真实 bucket 生效              | 需要真实 bucket 与凭据                         | **未验证**（CI 指向本地目录） |
| Uptime Kuma 因心跳缺席而告警（「没备份」那条路径）                 | 需要真实 Kuma 实例                             | **未验证**                    |
| `monitor.sh`/`install-beszel.sh` 语法、shellcheck、CRLF、unit 语法 | CI `Monitor and logs` lint                     | CI 每次推送                   |
| journald 上限被 systemd 识别、PG 日志参数被真实 postgres 接受      | CI `Monitor and logs` lint                     | CI 每次推送                   |
| **四类告警真的到达 sink**（磁盘/内存/服务停止/崩溃循环）           | CI `Monitor and logs` `alerting` 捕获端点      | **已验证**                    |
| 告警去重（cooldown）与恢复通知                                     | CI `Monitor and logs` `alerting`               | **已验证**                    |
| `install-beszel.sh` 校验和缺失/不匹配时拒绝                        | CI `Monitor and logs` lint（假制品）           | **已验证**                    |
| Beszel 真二进制运行、hub UI 可访问、真实内存占用                   | 需要真实服务器                                 | **未验证**（CI 用假制品）     |
| systemd timer 每分钟触发、journald 真机满盘 vacuum                 | 需要真实服务器                                 | **未验证**                    |
| 真实 webhook 频道（Slack/ntfy/…）收到告警                          | 需要真实 webhook                               | **未验证**                    |

> 本地开发机是 Windows，无法运行 systemd 或 Caddy，所以上述"未验证"项只能靠 CI 或真实服务器确认。
> `deploy.sh` 现在不再只靠语法检查：`Release` 工作流在 Linux runner 上用**假 systemd + 真二进制 + 真
> PostgreSQL** 完整跑一遍（下载 → 校验 → 解包 → 切换软链接 → `systemctl restart` → 轮询 `/health`），
> 见 `.github/workflows/release.yml` 的 `smoke` 任务。仍属"未验证"的是真实服务器上的证书签发、
> 真实 systemd/cgroup 限额、以及发布标签这条路径（仓库还没有标签）。

---

## 14. 文件清单

| 文件                                                                  | 作用                                                                 |
| --------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `deploy/systemd/jiuyue.service`                                       | 后端 unit：非特权用户、资源上限、加固、PG 就绪门禁                   |
| `deploy/systemd/drop-ins/postgresql@16-main.service.d/10-memory.conf` | PostgreSQL 内存上限与 OOM 保护                                       |
| `deploy/systemd/drop-ins/caddy.service.d/10-jiuyue-env.conf`          | Caddy 的环境文件与内存上限                                           |
| `deploy/Caddyfile`                                                    | 自动 HTTPS + `/api` 反向代理 + `/ws` 调优 + 静态 SPA                 |
| `deploy/deploy.sh`                                                    | 唯一发布入口：校验制品、切换、重启、健康检查、失败回滚               |
| `deploy/provision.sh`                                                 | 一次性幂等初始化（包/用户/目录/swap/PG/Caddy/密钥/unit）             |
| `deploy/postgresql/jiuyue-tuning.conf`                                | PostgreSQL 16 调参 drop-in                                           |
| `deploy/env/jiuyue.env.example`                                       | 运行时变量模板（占位符，无真实密钥）                                 |
| `deploy/backup.sh`                                                    | 每日备份：dump + 文件存储 + 加密 + 上传 + 分离保留 + 心跳            |
| `deploy/restore.sh`                                                   | 从备份集恢复数据库与文件存储（sha256 不匹配即拒绝）                  |
| `deploy/restore-drill.sh`                                             | 恢复演练：销毁 → 从远端恢复 → 逐字节比对                             |
| `deploy/env/backup.env.example`                                       | 备份配置模板（保留天数、远端、心跳 URL；占位符）                     |
| `deploy/systemd/jiuyue-backup.service` / `.timer`                     | 每日 18:30 UTC 执行备份（systemd timer，非 cron）                    |
| `docs/backup.md`                                                      | 备份与恢复的完整设计与诚实清单                                       |
| `docs/drills/2026-09-17-restore-drill.md`                             | 最近一次恢复演练的日期化记录                                         |
| `.github/workflows/deploy-validate.yml`                               | 每次推送校验上述全部制品                                             |
| `.github/workflows/backup-restore.yml`                                | 每次推送跑真恢复演练与失败告警断言                                   |
| `.github/workflows/release.yml`                                       | 构建发布制品（tar.gz + `.sha256`）、冒烟真跑、标签时发 Release       |
| `deploy/monitor/monitor.sh`                                           | 每分钟的磁盘/内存/服务/崩溃循环检查 + webhook 告警 + 状态记录        |
| `deploy/systemd/jiuyue-monitor.service` / `.timer`                    | 每分钟执行 `monitor.sh all`（oneshot，绝不自旋）                     |
| `deploy/monitor/install-beszel.sh`                                    | 原生安装 Beszel hub/agent（校验和不匹配即拒绝）                      |
| `deploy/systemd/beszel-hub.service` / `beszel-agent.service`          | 资源使用视图（Beszel）的 systemd 单元                                |
| `deploy/systemd/journald.conf.d/10-jiuyue-limits.conf`                | journald 显式容量/保留上限（后端 + Caddy 的日志）                    |
| `deploy/postgresql/jiuyue-logging.conf`                               | PostgreSQL 日志量限制（`logging_collector=off`，轮转交给 logrotate） |
| `deploy/logrotate/jiuyue-postgresql`                                  | PostgreSQL 日志 `size 50M` × `rotate 5` 轮转（接管 Debian 的配置）   |
| `deploy/env/monitor.env.example` / `beszel.env.example`               | 监控与视图配置模板（占位符，无真实 URL/密钥）                        |
| `docs/monitoring.md`                                                  | 监控、告警、日志轮转的完整设计与 runbook                             |
| `.github/workflows/monitor-validate.yml`                              | 校验配置并**触发/捕获**四类告警                                      |
