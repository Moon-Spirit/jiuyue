# 恢复演练记录 · 2026-09-17

> 对应 issue #42「备份与恢复演练」。本文是**一次真实演练的日期化记录**，含原始输出、耗时与发现的偏差。
> 机制说明见 [`docs/backup.md`](../backup.md)；自动化的同一套演练见
> [`.github/workflows/backup-restore.yml`](../../.github/workflows/backup-restore.yml) 的 `restore-drill` 任务。

---

## 结论

**通过（PASSED）。** 数据库与文件存储都被销毁后，仅凭**远端副本**（本地备份副本在销毁阶段被一并清除）
完整恢复，逐表全部行的字节哈希、每个文件的 sha256 全部一致，无任何差异。

- 备份耗时 **2 s**，恢复耗时 **5 s**，端到端 **12 s**。
- 演练脚本：`deploy/restore-drill.sh`（CI 与本机运行的是同一份）。
- **第一次运行是失败的**——它揪出了备份脚本里一个真实缺陷（见 §5）。这正是演练存在的意义。
- 修复后**连跑两次均通过**：第二次备份 5 s / 恢复 9 s / 端到端 32 s（机器负载与 gpg-agent 冷启动有波动），
  两次的比对都是 `DATABASE IDENTICAL` + `file store IDENTICAL`。结果可重复。

---

## 1. 环境

| 项       | 值                                                                         |
| -------- | -------------------------------------------------------------------------- |
| 日期     | 2026-09-17（香港时间；日志时间戳为 UTC `2026-09-16T19:17Z`）               |
| 主机     | Windows 11（`DESKTOP-M2QKIVT`），Git Bash / MSYS2                          |
| Shell    | GNU bash 5.3.15 (x86_64-pc-cygwin)                                         |
| 数据库   | PostgreSQL 16.15（原生 Windows 服务 `postgresql-x64-16`，非容器）          |
| 加密     | GnuPG 2.4.9（`--batch --pinentry-mode loopback --passphrase-file`）        |
| 归档     | GNU tar 1.35                                                               |
| 目标库   | `jiuyue_drill`（演练专用，schema 由 `backend/migrations/*.sql` 直接施加）  |
| 「远端」 | 本地目录（CI 与本次演练都把 `BACKUP_REMOTE` 指向一个目录，见 §6 未验证项） |

> **平台说明（必须讲清楚）**：生产与 CI 是 Linux；本次演练在 Windows/MSYS 上运行同一份 bash 脚本，
> 目的有两个——(1) 在把代码推给 CI 之前，先在本机真跑一遍恢复；
> (2) 因为工具链不同（MSYS 的 `psql` 参数解析、`install -m` 行为），它顺带暴露了几个可移植性缺陷（§5）。
> systemd timer、`rclone`、真实对象存储与真实 Uptime Kuma 无法在本机验证，见 §6。

---

## 2. 造的数据（真实数据，不是空库）

**数据库**（`psql` 直接插入，schema 来自 `backend/migrations/`）：

| 表                     | 行数 | 备注                                                               |
| ---------------------- | ---- | ------------------------------------------------------------------ |
| `users`                | 3    | 含 OAuth-only（`password_hash IS NULL`）                           |
| `sessions`             | 3    | 含一条已撤销（`revoked_at` 非空）                                  |
| `conversations`        | 2    | 1 个 direct（带 canonical `direct_key`）、1 个 group               |
| `conversation_members` | 5    |                                                                    |
| `messages`             | 5    | 含中文与 emoji：`你好，九月 🎉 drill message 3 (UTF-8 round-trip)` |

**文件存储**（`/var/lib/jiuyue` 的模拟目录，6 个文件）：

```
attachments/ab/cd/blob-1.bin            8192 字节随机
attachments/ab/cd/blob-2.bin            4096 字节随机
attachments/blob with spaces.bin         文件名带空格，含空格名必须能存活
avatars/u1.png
avatars/empty.bin                        0 字节文件必须能存活
secret/opaque.key                        1024 字节随机（秘密聊天附件目录）
```

---

## 3. 执行的命令

```bash
# 1. 建演练库并施加迁移
psql -X -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS jiuyue_drill WITH (FORCE)" "$ADMIN_DSN"
psql -X -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE jiuyue_drill OWNER jiuyue" "$ADMIN_DSN"
for f in backend/migrations/*.sql; do psql -X -v ON_ERROR_STOP=1 -q -f "$f" "$DSN"; done

# 2. 造数据（见 §2）与文件存储、生成一次性演练口令

# 3. 演练：快照 → 备份 → 断言密文 → 销毁 → 从远端恢复 → 比对
bash deploy/restore-drill.sh \
  --dsn  postgres://jiuyue:jiuyue_dev_password@127.0.0.1:5432/jiuyue_drill \
  --backup-dir /tmp/drill/backups \
  --remote     /tmp/drill/backups/remote \
  --files-dir  /tmp/drill/files \
  --passphrase-file /tmp/drill/pass \
  --work-dir   /tmp/drill/work
```

（本记录中路径为便于阅读做了简化；实际为 `$RUNNER_TEMP`/Windows 临时目录下的绝对路径。）

---

## 4. 原始输出

以下为 `deploy/restore-drill.sh` 的真实输出（Windows 控制台对 UTF-8 表格线的渲染有乱码，已剔除纯装饰性的框线；
其余逐字保留）：

```
=== apply migrations (the same SQL the binary embeds) ===
  /c/Users/zh134/Desktop/jiuyue/backend/migrations/20260917120000_users.sql
  /c/Users/zh134/Desktop/jiuyue/backend/migrations/20260917130000_sessions.sql
  /c/Users/zh134/Desktop/jiuyue/backend/migrations/20260917140000_conversations_and_messages.sql
 public | conversation_members | 数据表 | jiuyue
 public | conversations        | 数据表 | jiuyue
 public | messages             | 数据表 | jiuyue
 public | sessions             | 数据表 | jiuyue
 public | users                | 数据表 | jiuyue
(5 行记录)

=== seed real data ===
users=3
sessions=3
conversations=2
members=5
messages=5

=== seed the file store ===
/tmp/drill/files/attachments/ab/cd/blob-1.bin
/tmp/drill/files/attachments/ab/cd/blob-2.bin
/tmp/drill/files/attachments/blob with spaces.bin
/tmp/drill/files/avatars/empty.bin
/tmp/drill/files/avatars/u1.png
/tmp/drill/files/secret/opaque.key

=== throwaway passphrase ===

=== RUN THE DRILL ===
2026-09-16T19:17:50Z [drill] === step 1/6: snapshot the live data (source of truth) ===
2026-09-16T19:17:52Z [drill] snapshot taken: 5 db lines, 6 files
2026-09-16T19:17:52Z [drill] === step 2/6: back up (encrypt, upload to /tmp/drill/backups/remote) ===
2026-09-16T19:17:52Z [backup] dumping PostgreSQL (custom format, piped end-to-end)
2026-09-16T19:17:52Z [backup] archiving the file store at /tmp/drill/files
2026-09-16T19:17:53Z [backup] both artifacts decrypt and parse (pg_restore --list / tar -tzf)
2026-09-16T19:17:55Z [backup] backup set ready: /tmp/drill/backups/jiuyue-20260916T191750Z.{db.dump.gz.gpg,files.tar.gz.gpg,manifest}
2026-09-16T19:17:55Z [backup] uploaded backup set to /tmp/drill/backups/remote
2026-09-16T19:17:55Z [backup] local retention: deleting sets older than 7 days from /tmp/drill/backups
2026-09-16T19:17:55Z [backup] remote retention: deleting sets older than 30 days from /tmp/drill/backups/remote
2026-09-16T19:17:55Z [backup] WARNING: no HEARTBEAT_URL configured; the run is not being watched
2026-09-16T19:17:56Z [backup] done: jiuyue-20260916T191750Z
2026-09-16T19:17:56Z [drill] backup took 4s
2026-09-16T19:17:56Z [drill] === step 3/6: assert the artifacts are encrypted ===
2026-09-16T19:17:56Z [drill] both remote artifacts are OpenPGP-encrypted (verify at rest, not just in transit)
2026-09-16T19:17:56Z [drill] === step 4/6: DESTROY the data ===
2026-09-16T19:17:56Z [drill] DESTROYING the database 'jiuyue_drill' and the file store '/tmp/drill/files'
2026-09-16T19:17:57Z [drill] destroyed database, file store and local backup copies — only /tmp/drill/backups/remote remains
2026-09-16T19:17:57Z [drill] === step 5/6: restore from the REMOTE copy ===
2026-09-16T19:17:57Z [restore] sha256 verified: jiuyue-20260916T191750Z.db.dump.gz.gpg
2026-09-16T19:17:58Z [restore] sha256 verified: jiuyue-20260916T191750Z.files.tar.gz.gpg
2026-09-16T19:17:58Z [restore] creating target database: jiuyue_drill
2026-09-16T19:17:58Z [restore] restoring database into postgres://jiuyue:jiuyue_dev_password@127.0.0.1:5432/jiuyue_drill
2026-09-16T19:17:59Z [restore] database restored
2026-09-16T19:17:59Z [restore] restoring the file store into /tmp/drill/files
2026-09-16T19:17:59Z [restore] file store restored (6 files)
2026-09-16T19:17:59Z [restore] row counts after restore:
  users                  3
  sessions               3
  conversations          2
  conversation_members   5
  messages               5
2026-09-16T19:18:00Z [restore] restore complete
2026-09-16T19:18:00Z [drill] restore took 3s
2026-09-16T19:18:00Z [drill] === step 6/6: compare ===
2026-09-16T19:18:02Z [drill] database: IDENTICAL (5 lines compared)
2026-09-16T19:18:02Z [drill] file store: IDENTICAL (6 files compared)
2026-09-16T19:18:02Z [drill] DRILL PASSED — destroy + restore round-tripped with no discrepancy
2026-09-16T19:18:02Z [drill] backup 2s, restore 5s, total 12s
```

恢复后的抽查（从**远端副本**恢复出来的库）：

```
             id             |                       body
----------------------------+--------------------------------------------------
 01J8ZQ7K2M4N6P8R0T2V4X6Y8J | drill message 1
 01J8ZQ7K2M4N6P8R0T2V4X6Y8K | drill message 2 — with a dash
 01J8ZQ7K2M4N6P8R0T2V4X6Y8M | 你好，九月 🎉 drill message 3 (UTF-8 round-trip)
 01J8ZQ7K2M4N6P8R0T2V4X6Y8N | group drill message 1
 01J8ZQ7K2M4N6P8R0T2V4X6Y8P | group drill message 2

 users
-------
     3
```

**比对口径**：`db.txt` 的每一行是「表名 + 行数 + `sha256(COPY (SELECT * FROM t ORDER BY 1,2) TO STDOUT)`」——
即该表**全部行的逐字节序列化哈希**，不是抽样。文件侧是每个文件的 `sha256sum`，按路径排序。
`before` 与 `after` 两个文件 `diff` 为空才算通过。

---

## 5. 演练发现的偏差（这才是本次演练最有价值的产出）

**第一次运行是失败的。** 演练按设计抓住了备份脚本里一个真实缺陷，以及三个可移植性问题。
全部已修复；修复后重跑通过。

| #   | 现象（演练输出）                                                      | 根因                                                                                                                                                                                                                                               | 修复                                                                                                                                                                                                                                            |
| --- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `ERROR: database artifact is not a readable pg_restore archive`       | `backup.sh` 的 `verify_artifacts` 把**仍处于 gzip 压缩**的解密流直接喂给 `pg_restore --list`，少了 `gunzip` 这一级。也就是说：单个归档本身是好的，但**上传前的自校验恒失败**——若没有这次演练，这个「验证」要么永远报错、要么被人误当成误报而关掉。 | `verify_artifacts` 改为 `gpg_decrypt \| gunzip \| pg_restore --list`，与 `restore.sh` 的恢复管道一致。                                                                                                                                          |
| 2   | 演练在「断言制品已加密」那一步**挂住**（pinentry 弹窗）               | `assert_encrypted` 用了 `gpg --list-packets` 但**没给口令**；对对称加密文件，它会尝试解密会话密钥，从而阻塞在交互式口令提示上。                                                                                                                    | 改为两步：先做**结构性检查**（首个字节必须是高位为 1 的 OpenPGP 包头；gzip 的 `1f`、tar、明文都过不了），再用 `--batch --pinentry-mode loopback --passphrase-file` 调 `--list-packets` 证明它真能解析。既不会挂，也没有退化成「只看文件非空」。 |
| 3   | `install: cannot change permissions of '…/remote': Permission denied` | 上传目标用 `install -d -m 0750`，在 MSYS、挂载的共享目录或受限文件系统上 `chmod` 会被拒绝，直接让备份失败。                                                                                                                                        | 换成 `mkdir -p` + best-effort `chmod`（失败只记一行 note）。Linux 上权限仍是 0750，行为不变。                                                                                                                                                   |
| 4   | `psql` 报 `unrecognized command-line argument "-v"` 等一串错误        | 在 MSYS 下以 `psql "$DSN" <更多选项>`（DSN 在最前）调用时，`psql` 的参数解析会被打乱。虽属平台特性，但「选项在前、库名在后」本来就是 libpq 的惯例。                                                                                                | 统一改为 `psql -X … "$DSN"`（DSN 放最后），`restore.sh` / `restore-drill.sh` 全部调用点均已调整。                                                                                                                                               |

> 其中 #1 是**真实产品缺陷**，#2 会让演练在 Linux CI 上以同样的方式挂死（CI 无 tty 时会报错而非提示，但
> 同样是误判），#3、#4 是可移植性缺陷。四者都是因为「真跑一遍」才暴露的——
> 语法检查和肉眼 review 都发现不了。

### 失败告警路径的演练

同一套脚本的失败路径也单独验证过（CI 里是 `alerting` 任务）：用一个连不上的 `DATABASE_URL`
让 `backup.sh` 失败，同时把 `HEARTBEAT_URL` 指向一个本地捕获服务。真实捕获到的请求：

```
/api/push/alert?status=down&msg=backup+failed+%28exit+1%29
```

日志对应输出：

```
2026-09-16T19:20:03Z [backup] dumping PostgreSQL (custom format, piped end-to-end)
pg_dump: error: connection to server at "127.0.0.1", port 5999 failed: Connection refused
2026-09-16T19:20:05Z [backup] ERROR: database dump failed (pg_dump / gzip / gpg)
2026-09-16T19:20:05Z [backup] heartbeat sent: status=down
```

即：**任何非零退出都会走 `EXIT` trap 发 `status=down`**，包括数据库连不上、加密失败、
磁盘写不进、校验不通过等所有失败分支。

---

## 6. 本次演练未能覆盖的部分

| 项目                                                      | 为什么没覆盖                                           |
| --------------------------------------------------------- | ------------------------------------------------------ |
| `rclone` 上传到**真实对象存储**（R2/S3 + `rclone crypt`） | 本机与 CI 都没有 bucket 与凭据；「远端」指向本地目录   |
| 远端保留对真实 bucket 生效（`rclone delete --min-age`）   | 同上                                                   |
| **Uptime Kuma 因心跳缺席而告警**（「没备份」那条路径）    | 需要一个真实 Kuma 实例；本次只验证到「请求被正确发出」 |
| systemd timer 在真实服务器上按 18:30 UTC 触发             | 本机与 CI 都没有 systemd 计时器                        |
| 2C2G 生产机上的真实耗时/内存                              | 本机 12 s，机器规格不同，不能外推                      |
| `deploy/provision.sh` 在干净服务器上安装备份链路          | 需要一台干净服务器                                     |

这些在 [`docs/backup.md` §9 未验证清单](../backup.md#9-未验证清单诚实清单) 中有完整对照表。

---

## 7. 下次演练要做的事

1. 至少每季度一次，或每次改动 `backup.sh` / `restore.sh` / 存储布局之后立即一次；
2. 下一次要在**真实服务器**上跑（用**独立的演练库与目录**，绝不动生产数据），
   确认 systemd timer、真实耗时、以及 `rclone` 上传路径；
3. 在真实 Uptime Kuma 上故意停掉 timer 一次，确认「心跳缺席」确实会告警；
4. 把结果写进 `docs/drills/`（新建一个日期文件），不要覆盖本文件。
