# Local Dev Environment

Docker-free local development setup for this repo's host machine (Windows, PowerShell 5.1).

## PostgreSQL 16 (development database)

| Fact                | Value                                                                                                          |
| ------------------- | -------------------------------------------------------------------------------------------------------------- |
| Install method      | Native Windows install via `winget` — package `PostgreSQL.PostgreSQL.16` (EDB build), installed 2026-09-17     |
| Installed version   | PostgreSQL 16.15, compiled by Visual C++ build 1944, 64-bit                                                    |
| Binaries            | `C:\Program Files\PostgreSQL\16\bin\` — e.g. `C:\Program Files\PostgreSQL\16\bin\psql.exe` (not on `PATH`)     |
| Data directory      | `C:\Program Files\PostgreSQL\16\data`                                                                          |
| Port                | `5432` — **loopback only** (`listen_addresses = 'localhost'`)                                                  |
| Windows service     | `postgresql-x64-16` — Automatic startup, runs as `NT AUTHORITY\NetworkService`                                 |
| Superuser           | role `postgres`, password `postgres` (dev-only; chosen by the unattended installer)                            |
| App role / database | role `jiuyue` (password `jiuyue_dev_password`, attribute `CREATEDB` for sqlx test databases) owns `jiuyue_dev` |
| Connection string   | `postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_dev`                                              |
| Lifecycle script    | `scripts/pg-dev.ps1` (`start` / `stop` / `status`)                                                             |

### Start / stop / status

From the repo root:

```powershell
powershell -File scripts\pg-dev.ps1 status   # exit code 0 = up and accepting connections
powershell -File scripts\pg-dev.ps1 stop
powershell -File scripts\pg-dev.ps1 start
```

`start` / `stop` control a Windows service and therefore need an elevated shell; `status` does not.

### Connect

```powershell
$env:PGPASSWORD='jiuyue_dev_password'
& "C:\Program Files\PostgreSQL\16\bin\psql.exe" -d "postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_dev"
```

### Reinstall / recovery (if the install is lost and this file is all you have)

```powershell
winget install --id PostgreSQL.PostgreSQL.16 -e --accept-source-agreements --accept-package-agreements --disable-interactivity
# superuser-created after install, as postgres (password: postgres):
#   CREATE ROLE jiuyue LOGIN PASSWORD 'jiuyue_dev_password' CREATEDB;
#   CREATE DATABASE jiuyue_dev OWNER jiuyue;
# then set listen_addresses = 'localhost' in C:\Program Files\PostgreSQL\16\data\postgresql.conf
# and restart the postgresql-x64-16 service.
```

## Machine-specific notes (read before touching PostgreSQL on this host)

- **PostgreSQL 17 also exists** (`C:\Program Files\PostgreSQL\17`, service `postgresql-x64-17`) and is **not used by this project**. It used to own port 5432; it has been stopped and its startup type set to `Manual` so that PostgreSQL 16 can own 5432. To hand 5432 back to it: stop `postgresql-x64-16`, then `Start-Service postgresql-x64-17; Set-Service postgresql-x64-17 -StartupType Automatic`. Only one server can hold port 5432.
- The PG16 installer created the cluster with `UTF8` encoding and the OS locale collation (`Chinese (Simplified)_China.936`); this is dev-only and does not need to match production (PostgreSQL 16 on Linux).
- No TLS, no replication, no tuning — development database only.
