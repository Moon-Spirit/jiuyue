# 备份与恢复：加密备份、分离保留、心跳告警、真演练

> 面向单台 2 vCPU / 2 GB 香港云服务器（Debian/Ubuntu），**不使用任何容器**（[`docs/adr/0010-no-containers.md`](./adr/0010-no-containers.md)）。
> 策略依据 [`docs/research/2026-09-16-vps-2gb-ops-research.md`](./research/2026-09-16-vps-2gb-ops-research.md) §(f)，
> 其中 Docker 专属部分已随 ADR-0010 作废（脚本从 `docker compose exec pg_dump` 改为原生 `pg_dump`）。
> 文件存储必须一起备份：[`docs/adr/0006-local-disk-storage-abstraction.md`](./adr/0006-local-disk-storage-abstraction.md)。
> 部署全貌见 [`docs/deploy.md`](./deploy.md)。

---

## 0. 一句话

**没有被恢复过的备份只是假设。** 因此本套机制有一个硬性配套：一份可重复执行的恢复演练
（`deploy/restore-drill.sh`），它在 CI 里每次推送都真跑一遍——造数据、备份、**销毁**数据库与文件目录、
从**远端副本**恢复、逐表逐字节比对。没有它，`restorable` 这个词就没有证据。

---

## 1. 组件

| 文件                                      | 作用                                                                       |
| ----------------------------------------- | -------------------------------------------------------------------------- |
| `deploy/backup.sh`                        | 产出一份备份集：dump + 归档 + **加密** + 校验 + 上传 + 两套保留策略 + 心跳 |
| `deploy/restore.sh`                       | 从备份集恢复数据库与文件存储（sha256 不匹配就拒绝）                        |
| `deploy/restore-drill.sh`                 | 演练：快照 → 备份 → 销毁 → 从远端恢复 → 比对，退出码即结论                 |
| `deploy/env/backup.env.example`           | 配置模板（占位符；真值只在服务器 `/etc/jiuyue/backup.env`）                |
| `deploy/systemd/jiuyue-backup.service`    | oneshot：执行 `backup.sh`，失败即失败（不自动重试掩盖问题）                |
| `deploy/systemd/jiuyue-backup.timer`      | 每日 18:30 UTC（= 香港 02:30）触发，`Persistent=true` 补跑错过的窗口       |
| `.github/workflows/backup-restore.yml`    | lint + **真恢复演练** + 失败告警断言                                       |
| `docs/drills/2026-09-17-restore-drill.md` | 最近一次演练的日期化记录（含真实输出、耗时、发现的偏差）                   |

`deploy/provision.sh` 负责把脚本装到 `/usr/local/lib/jiuyue/`、写 `/etc/jiuyue/backup.env`、
生成加密口令到 `/etc/jiuyue/backup_gpg_pass`（0600）、装 unit 并 `enable --now` timer。

---

## 2. 一个备份集里有什么

一次运行产出五件东西，命名 `<stamp>` = `YYYYMMDDTHHMMSSZ`：

```
jiuyue-<stamp>.db.dump.gz.gpg            pg_dump -Fc | gzip -6 | gpg 对称加密
jiuyue-<stamp>.db.dump.gz.gpg.sha256     校验侧车（sha256sum 格式，与 deploy.sh 一致）
jiuyue-<stamp>.files.tar.gz.gpg          文件存储（/var/lib/jiuyue）tar -cz | gpg
jiuyue-<stamp>.files.tar.gz.gpg.sha256   校验侧车
jiuyue-<stamp>.manifest                  元数据（无密）：库名、pg_dump 版本、文件数、两个 sha256
```

两处刻意设计：

- **数据库单独一个制品、文件存储单独一个制品**，而不是打成一个大包。恢复时可以只恢复一半
  （`restore.sh --db-only` / `--files-only`），也便于分别核对。
- **`.sha256` 侧车格式与 `deploy/deploy.sh`、`.github/workflows/release.yml` 完全一致**
  （`<64-hex>` + 两个空格 + 文件名）。`restore.sh` 按 deploy.sh 的「sha256 不匹配就拒绝」纪律执行。

### 上传前的自校验

`backup.sh` 在**上传之前**会解密并解析两个制品：

- 数据库制品必须能被 `pg_restore --list` 读出（证明它是有效的 PostgreSQL 自定义格式归档）；
- 文件制品必须能被 `tar -tzf` 列出。

任何一个读不回来，该次运行失败、不产出「成功」的备份集。这是「可恢复」的第一道闸门——
它证明归档能解密、能解析，不证明数据内容正确；内容正确由演练证明（§7）。

---

## 3. 加密：离开机器之前就加密

- 算法：GPG 对称加密，`--cipher-algo AES256`，口令来自**文件** `GPG_PASSPHRASE_FILE`
  （默认 `/etc/jiuyue/backup_gpg_pass`，0600 root）。
- 管道直通，不在小磁盘上落未加密的大临时文件：`pg_dump … | gzip | gpg > …`。
- **恢复同样需要这个口令**。口令丢了，备份全部不可读；口令和备份放在同一块盘上，加密就是摆设。
  口令由 `provision.sh` 首次生成，**必须立刻抄进密码管理器**。
- 仓库里只有占位符，没有任何真实口令、bucket 名或密钥。

远端若是 `rclone crypt` 包装的对象存储（R2/S3/OSS），则内容、文件名、目录名都会在离开机器前被再加密一层
（research §(f) 第 3 条）。此时本地的 gpg 加密仍然保留——两层不冲突，本地加密保证「制品本身即密文」。

---

## 4. 保留策略：本地 7 天 / 远端 30 天，两套互相独立

| 位置                           | 保留                                     | 由谁清理                                                             | 为什么这么长                                       |
| ------------------------------ | ---------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------- |
| 本地 `BACKUP_DIR`（机箱内）    | `BACKUP_KEEP_LOCAL_DAYS`（默认 **7**）   | `backup.sh` 的 `prune_local`（`find -mtime`）                        | 本地盘便宜，应付误删；**一次磁盘故障就会全部消失** |
| 远端 `BACKUP_REMOTE`（机箱外） | `BACKUP_KEEP_REMOTE_DAYS`（默认 **30**） | `backup.sh` 的 `prune_remote`（rclone `--min-age` 或 `find -mtime`） | 这才是能活过机器故障的那份，保留更久               |

**为什么必须分开**（research §(f) 第 2 条）：

1. **上传用 `copy`，不用 `sync`。** `rclone sync` 会在本地 7 天文件被清理的瞬间把远端的 30 天历史一并删掉——
   清理一个位置就等于清理所有位置。用 `copy` 则远端只增不减，再由远端自己的策略按 30 天收敛。
2. **两个 prune 函数操作两个不同的根**（`prune_local` 只看 `BACKUP_DIR`，`prune_remote` 只看 `BACKUP_REMOTE`），
   代码上不存在一条路径能从本地删除远端。
3. **远端保留更长**，因为本地那份的价值随时间趋近于零（同一块盘、同一台机器、同一场灾难）。

两侧都可以用 `--no-prune` 临时关闭；生产上由 timer 每日跑，不需要人工干预。

---

## 5. 调度：systemd timer，不是 cron

`jiuyue-backup.timer` 每日 `18:30 UTC`（香港 02:30，最安静的时段）触发 `jiuyue-backup.service`。
选 systemd 而不是 cron 的理由：与项目其余部分一致（journald 收日志、`systemctl --failed` 看失败、
`Persistent=true` 自动补跑错过的窗口）。

`jiuyue-backup.service` **故意不写 `Restart=always`**：备份失败应该「保持失败、可见」
（`systemctl --failed`、`journalctl -u jiuyue-backup`），而不是静默循环重试。

---

## 6. 告警：心跳式，抓的是「没备份」而不是「备份失败」

**最危险的情况不是备份失败，而是备份根本没跑。** 失败会留下日志和失败的 unit；没跑则什么都不留。

因此每次运行都往一个 **Uptime Kuma push monitor** 打一次心跳（`docs/deploy.md` §12 推荐的轻量监控）：

| 结果             | 请求                                                  |
| ---------------- | ----------------------------------------------------- |
| 成功             | `GET <HEARTBEAT_URL>?status=up&msg=backup+<stamp>+ok` |
| **任何非零退出** | `GET <HEARTBEAT_URL>?status=down&msg=<原因>`          |

实现要点：`backup.sh` 用一个 `EXIT` trap 保证——无论在第几步失败、以什么码退出，`status=down` 都会发出去
（心跳发送本身是 best-effort，失败只记警告，绝不掩盖真正的退出码）。

**监控侧配置（关键）**：Uptime Kuma 里建一个 **Push** 类型 monitors，心跳间隔设成略大于 25 小时
（例如 25h），把生成的 push URL 填进 `/etc/jiuyue/backup.env` 的 `HEARTBEAT_URL`。
这样：

- 备份失败 → 立刻收到 `status=down`；
- timer 被禁用、机器关机、脚本被删、cron 被改 → **心跳缺席** → 超过间隔后 Uptime Kuma 报 down。

也就是说，「没人告诉你备份坏了」这件事本身就是告警。若 `HEARTBEAT_URL` 留空，一次失败只会在日志里
记一行 `ERROR: no HEARTBEAT_URL configured — this failure cannot alert anyone`——
**这意味着没有任何人会知道**，所以生产环境不允许留空。

`systemd` 那侧还有第二重信号：unit 失败会出现在 `systemctl --failed` 与 journald。

---

## 7. 恢复

### 7.1 用哪个备份集

```bash
sudo deploy/restore.sh --list      # 列出 BACKUP_DIR / BACKUP_REMOTE 里可用的集合
```

### 7.2 恢复数据库与文件存储

```bash
# 从远端副本恢复最新一个可用集合（先把 BACKUP_REMOTE 指向真实的 rclone remote 或目录）
sudo deploy/restore.sh --stamp 20260917T003000Z

# 只恢复数据库，恢复进一个空的校验库
sudo deploy/restore.sh --stamp <s> --db-only \
  --dsn postgres://jiuyue:...@127.0.0.1:5432/jiuyue_verify

# 只恢复文件存储到别处（先在沙箱里看一眼，不动生产目录）
sudo deploy/restore.sh --stamp <s> --files-only --files-dir /tmp/jiuyue-files-check
```

行为：

- **先校验 sha256 再恢复**，侧车缺失或不匹配直接拒绝（`--from` 可以是 rclone remote，会自动下载）。
- 目标库不存在时自动创建（需要 `CREATEDB`）；已存在且含 `users` 表时，必须显式 `--clean` 才覆盖。
- 恢复完打印各表行数，便于肉眼确认。

### 7.3 灾难恢复 runbook（无容器版）

1. 开一台新机，`deploy/provision.sh` 初始化（会装好 systemd、Caddy、PostgreSQL）；
2. 从**密码管理器**取回 `backup_gpg_pass` 与 rclone 配置——它们必须在机器之外还有一份；
3. `rclone copy` 最近一组备份到本地；
4. 把 `/etc/jiuyue/jiuyue.env`、`/etc/jiuyue/backup.env`、Caddyfile 从备份或密码管理器取回；
5. `sudo deploy/restore.sh --stamp <s>`（数据库 + 文件存储）；
6. 用 `deploy/deploy.sh` 发布一个版本，验证登录 + WebSocket 连接 + 一次写库；
7. **每季度演练一次**（见 §8）。演练本身就是交付物。

---

## 8. 演练：`restore-drill.sh` 与 CI

`deploy/restore-drill.sh` 是一次可重复的「破坏性」演练：

1. 快照线上数据（表集合 + 每表行数 + 每表全部行的规范化字节哈希；文件存储每个文件的 sha256）；
2. 跑 `backup.sh`（真加密、真上传到 `--remote`）；
3. 断言两个远端制品确实是 OpenPGP 密文；
4. **销毁**：`DROP DATABASE … WITH (FORCE)`、`rm -rf` 文件目录、**并清掉本地备份副本**——
   只留下 `--remote` 这一份；
5. `restore.sh --from <remote>` 从远端恢复；
6. 重新快照并 `diff`，任何差异即 FAIL，非零退出。

`.github/workflows/backup-restore.yml` 的 `restore-drill` 任务就是：装原生 PostgreSQL → 施加迁移 →
用 `psql` 造真数据（用户/会话/会话/参与者/消息，含中文与 emoji）→ 写真文件到文件存储 →
跑演练 → 断言日志里出现 `DESTROY` / `OpenPGP` / `database: IDENTICAL` / `file store: IDENTICAL` /
`DRILL PASSED`，并断言心跳捕获到 `status=up`。**任何一步被跳过都会红**，不允许「静默通过」。

同日还有一个 `alerting` 任务：用坏 `DATABASE_URL` 让 `backup.sh` 失败，断言退出码非零且心跳捕获到 `status=down`。

最近一次记录：[`docs/drills/2026-09-17-restore-drill.md`](./drills/2026-09-17-restore-drill.md)。

---

## 9. 未验证清单（诚实清单）

| 项目                                                            | 在哪里能验证                       | 状态                                      |
| --------------------------------------------------------------- | ---------------------------------- | ----------------------------------------- |
| 备份/恢复脚本语法、shellcheck、CRLF、systemd unit 语法          | CI `backup-restore` 的 lint 任务   | CI 每次推送                               |
| **销毁 → 从远端副本恢复 → 逐表逐字节一致**                      | CI `restore-drill` 任务 + 本机演练 | **已验证**（见 drills 记录）              |
| 文件存储被纳入备份（非仅数据库）                                | CI `restore-drill` 任务            | **已验证**（6 个文件含空文件、带空格名）  |
| 备份失败会发出 `status=down` 心跳                               | CI `alerting` 任务 + 本机捕获      | **已验证**                                |
| 制品在离开机器前已加密                                          | 演练中的结构性断言 + 本地捕获      | **已验证**（OpenPGP 包头 + 密文解析）     |
| **`rclone` 上传到真实对象存储**（R2/S3，含 `rclone crypt`）     | 需要真实 bucket 与凭据             | **未验证**——CI 把「远端」指向本地目录     |
| **远端保留策略对真实 bucket 生效**（`rclone delete --min-age`） | 需要真实 bucket                    | **未验证**——同上                          |
| **Uptime Kuma push monitor 真的会因心跳缺席而告警**             | 需要一个真实 Kuma 实例             | **未验证**——CI 只断言请求被发出           |
| systemd timer 在真实机器上按 18:30 UTC 触发                     | 需要真实服务器                     | **未验证**——本机与 CI 都无 systemd 计时器 |
| 2C2G 上的实际耗时与内存占用                                     | 需要真实服务器                     | **未验证**——本机演练 12 s，机器差异未知   |
| `provision.sh` 在干净服务器上安装备份链路                       | 需要真实服务器                     | **未验证**                                |

> 本机开发环境是 Windows（Git Bash + 原生 PostgreSQL 16.15 + GnuPG 2.4.9），
> Linux 专属部分（systemd timer、rclone、真实 bucket）无法在此确认——
> 这也是 CI 任务存在的意义，但 CI 同样没有真实 bucket 与真实 Kuma。

---

## 10. 常见操作

```bash
# 立即手动备份一次（先确认 backup.env 已填好）
sudo systemctl start jiuyue-backup.service
journalctl -u jiuyue-backup -n 100 --no-pager

# 看某天到底备了什么
sudo deploy/restore.sh --list

# 临时不清理任何东西地备份一次
sudo deploy/backup.sh --no-prune

# 演练（会销毁指定的库和目录，别指向生产）
sudo deploy/restore-drill.sh \
  --dsn postgres://jiuyue:...@127.0.0.1:5432/jiuyue_drill \
  --backup-dir /var/backups/jiuyue-drill \
  --files-dir /var/lib/jiuyue-drill \
  --passphrase-file /etc/jiuyue/backup_gpg_pass
```

区分点很重要：**`backup.sh` 只读生产数据，`restore-drill.sh` 会删除你给它的库和目录。**
不要把 `--dsn` / `--files-dir` 指向生产。
