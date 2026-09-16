# 全栈不使用容器：本地、CI、生产都用原生进程

Status: accepted

本项目**不使用 Docker 或任何容器运行时**。本地开发直接运行原生服务（PostgreSQL 装在宿主机上），CI 在 runner 上直接安装 PostgreSQL（不用 service container），生产环境用 **systemd 管理原生二进制 + Caddy 反向代理**。

## 理由

1. **内存预算**：部署目标是单台 2C2G 服务器。省掉 `dockerd` + `containerd` 常驻的约 100MB，把它留给 PostgreSQL 与后端进程。
2. **交付链路反而更短**：项目已经规定"不在服务器上编译"（2C2G 跑 LTO release 构建会 OOM）。那么链路本身就是 _CI 出制品 → 服务器拉制品 → 重启服务_，容器不自带镜像仓库也能完成，少一层。
3. **少一层故障域**：容器网络、存储驱动、cgroup 限额、iptables 与 UFW 的交互都不再是需要排查的对象。
4. **本地开发不再需要 WSL2 / Hyper-V**：Windows 上直接装原生 PostgreSQL 即可开工。

## Considered Options

- **Docker Compose（原计划）** — 否决：用户明确要求不用容器；且上述收益在单机场景下全部成立。
- **Podman** — 否决：同类容器运行时，不解决用户诉求。
- **WSL2 里跑 Docker** — 否决：本机未安装 WSL，且引入同样的容器层。

## Consequences

- 部署产物从"镜像"改为：**二进制 + 前端静态资源 + systemd unit + Caddyfile**。
- 需要自己管理进程生命周期（systemd unit）、日志（journald，天然带轮转）、资源限额（systemd 的 `MemoryMax` / `LimitNOFILE`）。
- 备份脚本直接操作原生 PostgreSQL（`pg_dump`）与文件目录，不再经过 `exec`。
- 回滚策略：保留上一版二进制，切换软链接后 `systemctl restart`。
- **Redis 不再是硬依赖**：单机阶段用进程内实现（presence / 限流 / 广播），多节点时再启用 Redis。这与 ADR-0002 与 ADR-0005 的"单机起步、可水平扩展"一致，也让本地开发只需 PostgreSQL 一个外部服务。
- **coturn 与媒体服务同样以原生方式运行**（ADR-0004 中"必须 host 网络"的约束随之消失，因为本就没有容器网络）。
- CI：GitHub Actions 在 runner 上 `apt-get install postgresql-16`（ubuntu 系 runner 自带 PostgreSQL 源），不使用 service container。
- `docs/research/2026-09-16-vps-2gb-ops-research.md` 中关于 Docker 的部分（内存限额、日志驱动、iptables）**不再是执行依据**，但其中 PostgreSQL 调参、swap、监控选型、备份策略仍然有效。

## 迁移余地

应用代码与部署方式解耦：后端只认环境变量（`DATABASE_URL` 等），前端只认 HTTP。将来若改用容器，只需重新打包，业务代码零改动。
