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

### 4.1 制品接口（由 ticket #44 的 CI 发布任务实现）

| 项     | 约定                                                             |
| ------ | ---------------------------------------------------------------- |
| 文件名 | `jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz`               |
| 内容   | `bin/jiuyue-server`（可执行）、`dist/`（`frontend/dist` 的内容） |
| 校验   | 同名 `.sha256` 伴随文件（`sha256sum` 格式）                      |
| 拉取   | HTTPS 下载，或本地路径                                           |

发布前可这样打包（示例，CI 里做；**不要在服务器上编译**）：

```bash
cargo build --manifest-path backend/Cargo.toml --release --bin jiuyue-server
pnpm --dir frontend build
mkdir -p pkg/bin pkg/dist
install -m 0755 backend/target/release/jiuyue-server pkg/bin/jiuyue-server
cp -a frontend/dist/. pkg/dist/
tar -C pkg -czf "jiuyue-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" bin dist
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

research §(f) 的策略在无容器下**依然成立**，只有脚本里的 `docker compose exec` 需替换为原生 `pg_dump`：

```bash
# 原生，无容器。全连接串强制 TCP，避免 cron 下的 peer 认证问题。
pg_dump -Fc --no-owner --no-privileges "$DATABASE_URL" \
  | gzip -6 \
  | gpg --batch --yes --passphrase-file /root/.backup_gpg_pass -c \
  > "/var/backups/pg/app_$(date -u +%Y-%m-%dT%H-%M-%S).dump.gz.gpg"
```

- 分层：Postgres 逻辑备份（本地 7 天 / 云端 30 天）、上传文件与媒体（`tar`/`restic`）、配置（本 `deploy/` 目录、`/etc/jiuyue/*`、Caddyfile）；
- 用 `rclone` + `rclone crypt` 上传（内容、文件名、目录名都加密），`rclone copy` 而非 `sync`，云端保留交给 bucket 生命周期；
- 给备份任务加**心跳监控**（Uptime Kuma push），静默即告警；
- 每季度在另一台机器演练恢复——**未演练的备份只是假设**。
- 注意：research §(f) 里灾难恢复步骤第 1、5 步提到 `docker`/`docker compose`，**已随 ADR-0010 作废**，
  改为原生安装 PostgreSQL 后 `pg_restore --clean --if-exists -d "$DATABASE_URL" < dump`。

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

## 12. 监控

按 research §(e) 的轻量结论，不要上 Prometheus+Grafana：

1. **Beszel agent**（+ hub，可在别处）——CPU/内存/磁盘/网络、阈值告警；
2. **Uptime Kuma `-slim` + SQLite**——HTTP 关键字检查公网入口、TCP 443、SSH，以及备份任务的 push 心跳
   （**不要**用内置 MariaDB、不要用 Chromium 浏览器监控，二者在 2 GB 机器上会 OOM）；
3. **外部检查**——用一个不在本机上的第三方检测公网 URL，机器整体挂掉时才有信号。

只开两条告警起步：**磁盘 > 85%** 与**关键服务不在运行**。另外盯：`systemctl status jiuyue` 的重启、
`journalctl` 里的 OOM、Postgres 连接占用率、持续 swap in/out、`MemAvailable` 低于 10–15%。

---

## 13. 验证状态（诚实清单）

| 检查项                                              | 在哪里验证                   | 状态        |
| --------------------------------------------------- | ---------------------------- | ----------- |
| Caddyfile 语法 / 结构                               | CI `caddy validate`          | CI 每次推送 |
| systemd unit 与 drop-in 语法                        | CI `systemd-analyze verify`  | CI 每次推送 |
| 部署脚本语法 / lint                                 | CI `bash -n` + `shellcheck`  | CI 每次推送 |
| 无 CRLF、env 模板变量齐全                           | CI                           | CI 每次推送 |
| 证书签发/续期、HTTP→HTTPS、真实域名路由             | 需要真实服务器与域名         | **未验证**  |
| `MemoryMax`/`LimitNOFILE` 的实际生效                | 需要真实服务器               | **未验证**  |
| `deploy.sh` 端到端（下载→校验→切换→重启→健康→回滚） | 需要真实服务器与 systemd     | **未验证**  |
| PostgreSQL 调参、swap 的实际表现                    | 需要真实服务器负载           | **未验证**  |
| Caddy 重载时的 WebSocket 保留行为                   | 需要真实浏览器与 live 连接   | **未验证**  |
| 制品打包内容（`bin/`+`dist/`）                      | 由 ticket #44 的发布任务产生 | **未验证**  |

> 本地开发机是 Windows，无法运行 systemd 或 Caddy，所以上述"未验证"项只能靠 CI 或真实服务器确认。
> `deploy.sh` 目前**只**通过语法/lint 检查，没有被真正执行过——这是已知缺口，建议后续加一个在 Linux runner 上
> 用假 systemd/假健康端点跑一遍的集成测试。

---

## 14. 文件清单

| 文件                                                                  | 作用                                                     |
| --------------------------------------------------------------------- | -------------------------------------------------------- |
| `deploy/systemd/jiuyue.service`                                       | 后端 unit：非特权用户、资源上限、加固、PG 就绪门禁       |
| `deploy/systemd/drop-ins/postgresql@16-main.service.d/10-memory.conf` | PostgreSQL 内存上限与 OOM 保护                           |
| `deploy/systemd/drop-ins/caddy.service.d/10-jiuyue-env.conf`          | Caddy 的环境文件与内存上限                               |
| `deploy/Caddyfile`                                                    | 自动 HTTPS + `/api` 反向代理 + `/ws` 调优 + 静态 SPA     |
| `deploy/deploy.sh`                                                    | 唯一发布入口：校验制品、切换、重启、健康检查、失败回滚   |
| `deploy/provision.sh`                                                 | 一次性幂等初始化（包/用户/目录/swap/PG/Caddy/密钥/unit） |
| `deploy/postgresql/jiuyue-tuning.conf`                                | PostgreSQL 16 调参 drop-in                               |
| `deploy/env/jiuyue.env.example`                                       | 运行时变量模板（占位符，无真实密钥）                     |
| `.github/workflows/deploy-validate.yml`                               | 每次推送校验上述全部制品                                 |
