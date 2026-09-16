# 监控、告警与日志轮转：2 GB 单机上的可观测性

> 面向单台 **2 vCPU / 2 GB / 100 Mbps** 香港云服务器（Debian/Ubuntu），**不使用任何容器**
> （[`docs/adr/0010-no-containers.md`](./adr/0010-no-containers.md)）。
> 选型依据 [`docs/research/2026-09-16-vps-2gb-ops-research.md`](./research/2026-09-16-vps-2gb-ops-research.md)
> §(e) 监控与 §(g) 日志（其 Docker 专属部分已随 ADR-0010 失效，内存数字仍然有效）。
> 部署全貌见 [`docs/deploy.md`](./deploy.md)，备份告警见 [`docs/backup.md`](./backup.md) §6。

---

## 0. 一句话

**`Restart=always` 会把崩溃藏起来。** 所以本套机制的核心不是"监控面板"，而是四个问题的答案：
磁盘是否将满、内存是否见底、关键服务是否停下、后端是否在**循环重启**——四者一旦成立，
必须有东西主动推给一个人类真的会看的地方（webhook）。日志则用一个共享上限兜住整台机器，
使得 `/` 的长期占用不随运行时间增长。

---

## 1. 组件

| 文件                                                         | 作用                                                               |
| ------------------------------------------------------------ | ------------------------------------------------------------------ |
| `deploy/monitor/monitor.sh`                                  | 四个检查 + 去重告警 + 状态记录；被 timer 每分钟调用一次            |
| `deploy/systemd/jiuyue-monitor.service` / `.timer`           | 每分钟跑一次 `monitor.sh all`（oneshot，绝不自旋）                 |
| `deploy/env/monitor.env.example`                             | 监控配置模板（webhook、阈值、窗口；占位符）                        |
| `deploy/monitor/install-beszel.sh`                           | 以**原生二进制**安装 Beszel hub/agent（校验和不匹配即拒绝）        |
| `deploy/systemd/beszel-hub.service` / `beszel-agent.service` | 资源使用视图（Beszel）的 systemd 单元                              |
| `deploy/env/beszel.env.example`                              | Beszel agent 的密钥/连接模板（占位符）                             |
| `deploy/systemd/journald.conf.d/10-jiuyue-limits.conf`       | journald 的**显式**容量/保留上限（后端 + Caddy 的日志都进这里）    |
| `deploy/postgresql/jiuyue-logging.conf`                      | PostgreSQL 文件日志的**有界**轮转（journald 抓不到它）             |
| `.github/workflows/monitor-validate.yml`                     | 每次推送：语法/systemd/配置校验 + **真触发一条告警并捕获 payload** |

`deploy/provision.sh` 负责把脚本装到 `/usr/local/lib/jiuyue/`、写 `/etc/jiuyue/monitor.env`、
创建 `/var/lib/jiuyue-monitor`、安装 unit、`enable --now jiuyue-monitor.timer`，并安装
journald/PG 两个日志配置。

---

## 2. 为什么是"脚本 + webhook + Beszel"，而不是 Prometheus/Grafana

research §(e) 已经把候选量过一遍，这里只做减法：

| 方案                                 | 常驻内存 / 代价                            | 结论                                                                                  |
| ------------------------------------ | ------------------------------------------ | ------------------------------------------------------------------------------------- |
| **Beszel**（hub + agent）            | agent ~10-30 MB；hub ~30-50 MB             | **选它**：CPU/内存/磁盘/网络历史 + 阈值告警 + webhook；SQLite；单文件二进制，容器无关 |
| Netdata                              | **150-400 MB RSS，~1.2 GB 磁盘/天**        | 否决：监控吃掉 2 GB 机器的三分之一内存，还天天写 1 GB 盘                              |
| Prometheus + node_exporter + Grafana | 3 个服务、3 个监听端口，且"与机器同生共死" | 否决：违背无容器与轻量目标；单机自监控讲不出"机器为什么死"                            |
| 自托管 Uptime Kuma                   | Node 运行时；60-100 MB，且有内存泄漏 issue | 不做常驻：无容器装 Node 代价大；**外部可达性**改用机器外的第三方检查                  |
| Glances                              | 50-150 MB（Python）                        | 只在 SSH 临时排障时用，不做常驻服务                                                   |

**告警不走 Beszel 的阈值通道，而是本仓库的 `monitor.sh`**，原因有二：其一，告警路由必须在 CI 里
可被**真实触发并捕获**（见 §10），第三方面板难做到这一点；其二，`Restart=always` 的崩溃循环、
systemd 服务状态这些是 systemd 语义，不是资源指标。Beszel 只负责"看"（历史与趋势）。

> **"机器整体挂掉"必须由机器之外的东西发现。** 一台自己给自报信的监控，在机器宕机时不会发出任何东西。
> 因此仍需一个**机器外**的第三方 HTTPS 检查打公网入口（research §(e) 第 3 条）；它不属于本仓库的交付物。

---

## 3. 内存预算算术（监控要花多少）

基线来自 [`docs/deploy.md`](deploy.md) §8：核心三服务上限和 1216 MB，加 OS 预留 350 MB ≈ 1566 MB，
相对 2048 MB 的**封顶剩余 ≈ 482 MB**（典型 RSS 之和 ≈ 885 MB，典型剩余 ≈ 1163 MB）。

| 监控组件                | `MemoryMax`（爆炸半径） | 典型 RSS      | 依据                                        |
| ----------------------- | ----------------------- | ------------- | ------------------------------------------- |
| Beszel agent            | 64 MB                   | 10-30 MB      | research §(e)；维护者称 agent <10 MB        |
| Beszel hub              | 96 MB                   | 50-100 MB     | research 给 30-50 MB，实测更高（见下）      |
| `monitor.sh`（oneshot） | 不设（瞬时进程）        | ~0 常驻       | 每次运行 < 1 s，只做 systemctl/df/一次 POST |
| **合计**                | **≈ 160 MB**            | **60-130 MB** | 占封顶剩余 482 MB 的 **12-27%**             |

- **hub 的内存实测比 research 的估计更高**：Beszel hub 基于 PocketBase；仓库 issue #1822 里给出
  **17 台机器、3 周运行 ≈ 54.63 MiB RSS**，同一 issue 里 **98 台机器**时涨到 **296-350 MiB**。
  本机只监控自己一台，按 50-100 MB 预算即可；但**不要把 hub 放在本机去管几十上百台**，那会吃掉全部余量。
- agent 官方没有公布 RSS，只声明"lightweight"；维护者在讨论里说 agent <10 MB。要把它写进自己的容量表，
  先量：`systemctl show -p MainPID --value beszel-agent | xargs -I{} grep VmRSS /proc/{}/status`。
- 上限之和接近 160 MB 仍然远低于 482 MB 的余量，且"典型"列才是必须装下的量（research §Resource budget
  同一逻辑）。
- hub 可以**移出本机**（research 允许"hub 放在更便宜的第二台机器"）。那样本机只剩 agent 的 10-30 MB。
  默认安装两者是为了开箱可用；介意这点内存就只跑 `--component agent`。
- **换来的是一只眼睛**：482 MB 预算里花掉约四分之一，就换到"磁盘将满/内存见底/服务停止/崩溃循环"
  四类告警与一张资源历史图。

---

## 4. 日志空间算术（`/` 为什么长期稳定）

三种日志生产者、两套机制，全部有**硬上限**：

| 生产者                      | 去向                                                                        | 上限机制                                          | 上限                          |
| --------------------------- | --------------------------------------------------------------------------- | ------------------------------------------------- | ----------------------------- |
| jiuyue-server               | journald（`StandardOutput=journal`）                                        | journald `SystemMaxUse`                           | 共享 200 MB                   |
| Caddy                       | journald（Caddyfile `output stderr`）                                       | journald `SystemMaxUse`                           | 同上（共享）                  |
| sshd / systemd / 容器外一切 | journald                                                                    | journald `SystemMaxUse`                           | 同上（共享）                  |
| PostgreSQL 16               | 文件 `/var/log/postgresql/…`（`pg_ctl -l` 重定向，`logging_collector=off`） | 本仓库接管的 logrotate（`size 50M` × `rotate 5`） | live ≤ 50 MB + 5 份 gzip 历史 |

**journald**（`deploy/systemd/journald.conf.d/10-jiuyue-limits.conf`）：

```
SystemMaxUse     = 200M      # /var/log/journal 的总预算
SystemMaxFileSize= 20M       # × SystemMaxFiles 10 = 200M
SystemMaxFiles   = 10
RuntimeMaxUse    = 64M       # /run（tmpfs）里的易失日志
SystemKeepFree   = 1G        # 绝不让 journal 把 / 用到只剩不足 1 GB
MaxRetentionSec  = 2week     # 即使没到 200M，超过两周也删
Storage          = persistent
```

**到达上限时会发生什么**：journald 按**从旧到新**的顺序删除已归档的 journal 文件（vacuum），
最新的事件永远保留。也就是说 journal 的磁盘占用是一个**固定 200 MB 的水池**，不是一条只增不减的曲线；
`SystemKeepFree=1G` 再叠加一层保护——当 `/` 的剩余空间将跌破 1 GB 时，journald 会提前收缩，
**宁可少记日志也不写满磁盘**。这正是"日志不会撑爆 `/`"的机制本身。

**PostgreSQL**（`deploy/postgresql/jiuyue-logging.conf` + `deploy/logrotate/jiuyue-postgresql`）：

先说一个容易踩的坑：**Debian 上 PostgreSQL 的标准日志不是它自己的 collector 写的。** `pg_ctlcluster`
用 `pg_ctl -l /var/log/postgresql/postgresql-<ver>-<cluster>.log` 把 stderr 重定向到一个 append 文件，
而 `logging_collector` 保持默认的 `off`。因此 **PostgreSQL 自己的 `log_rotation_size`/`log_rotation_age`/
`log_truncate_on_rotation` 在这个部署里根本不起作用**（它们只在 `logging_collector=on` 时生效）。
所以轮转交给 logrotate，conf.d 里只负责**限制日志量**（`log_statement='none'`、
`log_min_duration_statement=500ms`、`log_min_error_statement='error'`）。

轮转由本仓库接管：`deploy/logrotate/jiuyue-postgresql`（安装到 `/etc/logrotate.d/jiuyue-postgresql`）：

```
size 50M       # 活文件不会超过 ~50 MB
rotate 5       # 保留 5 份
compress       # 历史 gzip 压缩
copytruncate   # 拷贝后就地截断，PostgreSQL 无需 reload/restart
```

`provision.sh` 会把 Debian 自带的 `/etc/logrotate.d/postgresql-common` 移到
`…jiuyue-disabled`，因为 logrotate 对**同一个日志路径的重复条目会报错并跳过**，两份配置不能并存。
到达上限时：logrotate 拷贝文件 → 就地截断为 0 → 删除最旧的第 6 份。**上界 = ~50 MB 活文件 +
5 份 gzip 历史（通常数十 MB 以内）**，`/` 不随时间增长。

> 如果将来有人改成 `logging_collector = on`，PostgreSQL 会写到数据目录下（默认 `log_directory='log'`）
> 并生成带时间戳、**不断累积**的新文件——那就必须同时给那批文件一套操作者拥有的轮转策略。本项目故意保持 `off`。

**整机 `/` 上的日志上界 ≈ 200 MB（journald）+ ~50 MB（PG 活文件）+ 5 份压缩历史**。
`docs/deploy.md` §2 建议 `/` 至少 40 GB，故日志最多占其 1% 量级，且不再随时间增长。

> **为什么给后端和 Caddy 共享 200 MB 而不是各自设限**：单机应用量级很小，一个共享池实现简单、总量可控。
> 代价是流量爆发时 Caddy 访问日志会挤掉较旧的后端日志；实时看清当前状态用 `journalctl -u caddy -n` 与
> `journalctl -u jiuyue -n`，历史由 `MaxRetentionSec` 兜底。若日后访问日志确实需要独立预算，
> 可把 Caddy 的 `log` 改为 `output file` 并启用 Caddy 自身的 `roll_size`/`roll_keep`。

---

## 5. 告警目录：每条告警是什么意思、先做什么

`monitor.sh` 把每条告警 POST 成 JSON 到 `ALERT_WEBHOOK_URL`，字段见 `deploy/env/monitor.env.example`。
`event` 与 `severity` 是路由/分诊的依据：

| `event`                             | 触发条件                                                 | 第一动作                                                                                                  |
| ----------------------------------- | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `disk_usage`（warning，≥80%）       | 某个挂载点使用率跨过 `DISK_WARN_PERCENT`                 | `df -h`；`du -x -h -d1 / \| sort -h \| tail`；先清 journald/PG 旧日志与旧 release                         |
| `disk_usage`（critical，≥90%）      | 跨过 `DISK_CRIT_PERCENT`                                 | 同上但**立即**处理；确认 `/var/lib/jiuyue`（文件存储）没有异常增长；必要时扩容盘                          |
| `memory_available`（warning，≤15%） | MemAvailable 低于警告线                                  | `free -m`、`ps aux --sort=-rss \| head`；查是否有泄漏（应用 RSS 上限 320 MB）                             |
| `memory_available`（critical，≤8%） | 接近 OOM                                                 | 立刻看 `journalctl -k \| grep -i oom`；确认 swap 是否在换入换出（`vmstat 1`）                             |
| `service_down`                      | `systemctl is-active` 连续 `SERVICE_DOWN_STRIKES` 次失败 | `systemctl status <svc>`；`journalctl -u <svc> -n 100`；确认不是正在发布                                  |
| `service_restart_loop`              | 窗口内自动重启次数 ≥ `RESTART_THRESHOLD`                 | 看 `restarts.log` 的 `reason`；`journalctl -u jiuyue -n 200`；多半是新版本启动即崩，考虑 `deploy.sh` 回滚 |
| `test` / `recovery`                 | 合成测试；或某条件恢复                                   | 无需动作（`recovery` 表示对应问题已消失）                                                                 |

**阈值从宽到严、先少后多**（research §(e) 的告警纪律）：默认只开磁盘 80/90%、内存 15/8%、
服务停止、崩溃循环。告警疲劳是监控死亡的方式；加规则前先问"这条我能做什么"。

---

## 6. 崩溃循环与"正常发布重启"如何区分

`jiuyue.service` 的 `Restart=always` 让崩溃"看起来还活着"。区分手段有三层：

1. **计数语义**：`systemctl show -p NRestarts jiuyue` 统计的是 **`Restart=` 自动重启**次数；
   `deploy.sh` 执行的是显式 `systemctl restart`，会把计数器**归零**。于是"归零"读作一次人工/发布重启，
   "增长"读作自动重启。
2. **时间窗口 + 阈值**：`monitor.sh` 用 `journalctl -u jiuyue --since "-600s"` 统计 systemd 为每次自动重启
   打印的 **`Scheduled restart job`** 行数。**单次**重启（发布窗口内常见）不告警；窗口内 ≥ `RESTART_THRESHOLD`
   （默认 5 次/10 分钟）才告警——正是"发布重启正常、十分钟五次不正常"。
3. **记录而非仅存活**：每次观察到 `NRestarts` 变化，都会向
   `/var/lib/jiuyue-monitor/restarts.log` 追加一行，含 `kind=auto-restart|manual-or-deploy`、
   `NRestarts`、窗口内计数与 `reason`（取自 `Failed with result …` / `Main process exited …`）。
   于是"重启次数与原因"是**可查的记录**，不是一闪而过的日志。

```bash
# 看后端到底重启了几次、为什么：
systemctl show -p NRestarts,Result,ExecMainStatus jiuyue
cat /var/lib/jiuyue-monitor/restarts.log
journalctl -u jiuyue --since '-30 min' -o short-iso | grep -E 'Scheduled restart job|Failed with result|Main process exited'
```

---

## 7. 手工检查每个子系统（runbook）

```bash
# 监控自身是否在跑、上一轮是否成功
systemctl status jiuyue-monitor.timer jiuyue-monitor.service
systemctl list-timers jiuyue-monitor.timer
journalctl -u jiuyue-monitor -n 50 --no-pager
cat /var/lib/jiuyue-monitor/status.json      # 最近一轮的指标快照

# 手工跑一轮（立即验证告警链路；会真的 POST 到 webhook）
sudo systemctl start jiuyue-monitor.service
sudo /usr/local/lib/jiuyue/monitor.sh test-alert

# 磁盘 / 内存
df -h /; df -i /                              # 空间与 inode
free -m; vmstat 1 5                            # 内存与 swap 换入换出（si/so）
ps aux --sort=-rss | head -n 10

# 三个关键服务
systemctl is-active jiuyue caddy postgresql
systemctl status jiuyue --no-pager
journalctl -u jiuyue -n 100 --no-pager

# 日志占用与轮转是否生效
journalctl --disk-usage                       # 应 <= 200M
ls -lh /var/log/journal/                      # 已归档文件数量/大小
du -sh /var/log/postgresql; ls -lh /var/log/postgresql
sudo logrotate -d /etc/logrotate.d/jiuyue-postgresql  # 干跑：确认 size 50M / rotate 5 生效
systemd-analyze cat-config systemd/journald.conf | grep -E 'SystemMaxUse|MaxRetentionSec'

# 资源视图（Beszel）
systemctl status beszel-hub beszel-agent --no-pager
# hub 只监听 127.0.0.1:8090，从本机经 SSH 隧道访问：
#   ssh -L 8090:127.0.0.1:8090 ops@your-host  然后浏览器打开 http://localhost:8090
```

---

## 8. 安装与验证

```bash
# 1) 一次性：provision.sh 会装好日志配置与监控 timer
sudo bash deploy/provision.sh --domain chat.example.com --acme-email ops@example.com

# 2) 资源视图：原生安装 Beszel（校验和不匹配即拒绝；安装后自动 enable，需先配好 secret）
sudo bash deploy/monitor/install-beszel.sh --version <pinned>
#    编辑 /etc/jiuyue/beszel.env（KEY / HUB_URL / TOKEN），然后启动：
sudo systemctl restart beszel-hub beszel-agent
#    只在本机跑 agent、hub 放别处：
sudo bash deploy/monitor/install-beszel.sh --version <pinned> --component agent

# 3) 确认 timer 生效、日志上限生效
systemctl list-timers jiuyue-monitor.timer
journalctl --disk-usage
```

---

## 9. 未验证清单（诚实清单）

| 项目                                                                                 | 在哪里能验证                            | 状态                                        |
| ------------------------------------------------------------------------------------ | --------------------------------------- | ------------------------------------------- |
| `monitor.sh` 语法、shellcheck、CRLF、systemd unit 语法                               | CI `Monitor and logs` 的 lint           | CI 每次推送                                 |
| journald drop-in 被 systemd 识别、PG 日志参数被 postgres 接受                        | CI lint（`cat-config` / `postgres -C`） | CI 每次推送                                 |
| PG logrotate 配置语法与 `size 50M` 上限                                              | CI lint（`logrotate -d` 干跑）          | CI 每次推送                                 |
| **四类告警真的到达 sink**（磁盘/内存/服务停止/崩溃循环）                             | CI `alerting` 捕获端点                  | **已验证**（CI 每次推送，见 §10）           |
| 告警去重（cooldown 内不重复）与恢复通知                                              | CI `alerting`                           | **已验证**                                  |
| `install-beszel.sh` 校验和缺失/不匹配时拒绝安装                                      | CI lint                                 | **已验证**（用假制品）                      |
| Beszel 真二进制在真机上跑、hub web UI 可访问、真实内存占用                           | 需要真实服务器与网络                    | **未验证**（CI 用假制品测安装逻辑）         |
| webhook 真实到达人所看的频道（Slack/ntfy/…）                                         | 需要真实 webhook                        | **未验证**（CI 指向本地捕获端点）           |
| systemd timer 在真机上每分钟触发                                                     | 需要真实服务器                          | **未验证**（本机 Windows 与 CI 都无计时器） |
| journald `SystemMaxUse` 在真机满盘时的 vacuum 行为                                   | 需要真实服务器                          | **未验证**（CI 只断言配置被加载）           |
| 真实服务器上 PG 的 logrotate 实际轮转（文件被截断、旧份被删）                        | 需要真实服务器长期运行                  | **未验证**（CI 只做 `logrotate -d` 干跑）   |
| `provision.sh` 在真机上把 Debian 的 postgresql-common logrotate 移走且无重复条目错误 | 需要真实服务器                          | **未验证**                                  |
| 外部第三方检查"机器是否还活着"                                                       | 需要机器外的检查服务                    | **未验证**（不在本仓库交付范围）            |

> 开发机是 Windows，无法运行 systemd；Linux 专属行为只能靠 CI 或真实服务器确认。
> CI 能证明的是"脚本逻辑与配置语法正确、且一条被触发的告警确实到达了 sink"，
> 不能证明真实机器的计时器、真实 webhook、真实内存数字。

---

## 10. CI 证据：告警不是"配置了"，而是"被触发并捕获了"

`.github/workflows/monitor-validate.yml` 的 `alerting` 任务照 `backup-restore.yml` 的做法，
起一个本地捕获端点，然后**故意制造**四类条件，断言捕获到的 JSON：

| 场景     | 故意制造的条件                                         | 断言                                                   |
| -------- | ------------------------------------------------------ | ------------------------------------------------------ |
| 磁盘     | `DISK_WARN_PERCENT=0 DISK_CRIT_PERCENT=0`              | 捕获到 `"event":"disk_usage"`                          |
| 内存     | 假 `/proc/meminfo` + `MEM_AVAILABLE_*_PERCENT=100`     | 捕获到 `"event":"memory_available"`                    |
| 服务停止 | `systemctl` 桩返回 inactive + `SERVICE_DOWN_STRIKES=1` | 捕获到 `"event":"service_down"`                        |
| 崩溃循环 | `journalctl` 桩吐出 6 条 `Scheduled restart job`       | 捕获到 `"event":"service_restart_loop"`，且带 `reason` |
| 去重     | 同一条件再跑一次                                       | `service_down` 只出现 **1** 次                         |
| 无假告警 | 阈值拉高、全新状态目录，跑真实 `df`/`/proc`            | 捕获总数不增加                                         |

任何一步被注释、被跳过、被 `if: false` 掉，断言就会失败——不允许"静默通过"。

`lint` 任务里，日志那侧的同等证据是：`systemd-analyze cat-config systemd/journald.conf` 必须打印出我们的
`SystemMaxUse=200M` 等键（证明 drop-in 真被 systemd 读取）；一个真实 `postgres` 必须接受
`deploy/postgresql/jiuyue-logging.conf` 的每个参数（`postgres -C`）；`logrotate -d` 必须在
`/var/log/postgresql/*.log` 上以 `50M` 触发（证明字节上限配置有效）。这几步同样没有跳过分支。
