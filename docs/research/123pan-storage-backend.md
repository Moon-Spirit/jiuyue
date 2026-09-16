# 123云盘 (123pan) as a `Storage` backend for jiuyue — viability research

> Status: research artifact. Feeds an ADR on the storage-backend decision.
> Researched: 2026-09-16. Primary sources only (official docs, official ToS, upstream SDK repos, upstream issue threads).

---

## Verdict (read this first)

**Do not use 123云盘 as a `Storage` implementation for jiuyue.**

Three independent blockers, any one of which is disqualifying:

1. **ToS / business-model conflict (fatal).** 123云盘's open platform is explicitly designed for the model *"the developer's end users log into their own 123云盘 account"* — not *"the app owns one account and stores all its users' data in it."* Their own public statement names the exact pattern we would be implementing as the abuse they are actively shutting down: *"利用平台规则漏洞，进行批量注册、破解接口、盗链建站，甚至搭建影视网站、小文件下载站等商业服务"*. The developer agreement also forbids using the data interface to *"开展与西安一二三云计算有限公司存在竞争关系的业务"* — and serving media from their disk IS competition with their paid 直链/CDN product.
2. **Egress metering destroys the economics.** Every byte your end users download is billed against an extraction quota (free tier: 10 GB/month total, all clients combined; overage 0.05 CNY/GB; no-login download was removed entirely). A chat app with image/voice/video attachments is egress-dominated. `download_info` returns error `5113` when the quota is exhausted. There is no unmetered path.
3. **Rate limits are incompatible with a chat workload.** Documented free-tier QPS: `api/v1/file/list` = **0.2** (one request per 5 s), `api/v1/file/delete` = **1**, `api/v1/file/move` = **3**, `api/v2/file/list` = 5. Also a hard cap of **3 concurrent `access_token`s per `client_id`**. And the credential is now paywalled: you must buy a 开发者权益包 first.

There is **no official Rust SDK**, **no S3-compatible endpoint**, and **no OSS project that uses 123pan as a general application storage backend**. The two "real" integrations found (`rclone` backend, `restic-123pan`) are respectively **not merged upstream** and **built on the reverse-engineered web API**, not this API.

**Recommended path:** keep local disk → S3-compatible (R2/MinIO/COS) behind `Storage`. If a China-friendly, no-ICP-filing object store is the goal, that is what S3-compatible providers are for. 123pan does not occupy that niche and its ToS forbids treating it as if it did.

---
## (a) Official open platform & auth flow

### Where it lives

| Thing | URL |
| --- | --- |
| Open platform landing | `https://www.123pan.com/open` (also `https://www.123pan.cn/developer`) |
| **Official API documentation (the real one)** | `https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced` — a third-party Yuque book, not an `open.*` subdomain |
| API base URL | `https://open-api.123pan.com` |
| OAuth/authorize base (第三方挂载) | `https://www.123pan.com` |
| Web/app (unofficial) API base | `https://yun.123pan.com` + `/b/api`; login `https://login.123pan.com/api/user/sign_in` |
| Developer service agreement | `https://www.123pan.com/DeveloperAgreement` |
| Direct-link console | `https://www.123pan.com/DirectLink` |

There is **no `open.123pan.com`** — `https://open.123pan.com/` returns 404. The docs living on a third-party Yuque tenant (`123yunpan.yuque.com`) is itself a signal about the platform's maturity.

### Two distinct credential systems — do not conflate them

| | **`client_id` / `client_secret`** (API-interface access) | **`appId` / `secretId`** (第三方挂载应用接入 / OAuth) |
| --- | --- | --- |
| Purpose | Server-to-server: *the developer's own* 123pan account | *End users* authorise your app against *their* 123pan account |
| Who can apply | anyone who buys 开发者权益包 | **企业 only** — *"暂不支持个人开发者接入"* |
| How | buy 开发者权益包 → `client_id`/`client_secret` arrive as a **站内信 (in-site message)** | email `bd@123pan.com` with ICP filing + business licence; 7 working days; they issue `appId`/`secretId` |
| Token endpoint | `POST /api/v1/access_token` | `POST /api/v1/oauth2/access_token` |
| Rate limit | QPS 8 (free) / 10 (VIP) | 100/min per IP |

Credential purchase, verbatim from the official 接入流程 page:

> **# 1. 购买开发者权益包** — 请您阅读开发者服务协议，并开通开发者权益包，开通后会将 `client_id` 和 `client_secret` 以"站内信-消息"形式发送给您
> — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/hpengmyg32blkbg8>

Enterprise-only OAuth path, verbatim:

> **## 资质认证** — 对接应用需提供如下应用信息：1. 产品简介、产品用户量、日活、付费服务等 2. 对接应用的ICP备案信息、营业执照等复印件（暂不支持个人开发者接入）… 如上信息发送至123云盘商务邮箱 bd@123pan.com，等待审核，审核结果会在7个工作日内答复
> — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/kf05anzt1r0qnudd>

So: **personal accounts do not qualify for the third-party-app (OAuth) integration.** They formerly did — the OpenList thread documents the door closing:

> 有需要挂载123云盘的用户立即前往下方的地址申请开放平台API，**现在申请没有审核**，能立刻拿到访问凭证/刷新令牌 … 已经需要审核了😭 … 秒过的
> — <https://github.com/OpenListTeam/OpenList/issues/49>

By March 2026 the paywall was confirmed by the rclone backend author:

> but now the Official do not order the OpenAPI key with **unpaid method**, so might cannot continue test.
> — <https://github.com/rclone/rclone/pull/9047>

An independent integrator states the consequence of skipping payment:

> 由于123云盘架构政策重大调整，目前开放平台的直链及 API 提取额度**已全线转为付费高级属性**。您必须首先前往 123云盘主站的 开发者权益专区 购买订阅对应的"开发者权益包"。（注：若未打通付费权限通道直接填入尝试调用，后台请求将被严防死守并无限提示 `获取列表异常: 无效的登录信息`。）
> — <https://github.com/sakuradairong/go-123pan-pic>

### Token issuance

```
POST https://open-api.123pan.com/api/v1/access_token
Headers: Platform: open_platform ; Content-Type: application/json
Body:    { "clientID": "...", "clientSecret": "..." }

200 OK
{ "code": 0, "message": "ok",
  "data": { "accessToken": "eyJhbGciOiJIUzI1NiIs...", "expiredAt": "2025-03-23T15:48:37+08:00" },
  "x-traceID": "..." }
```
— <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/gn1nai4x0v0ry9ki>

**Token lifetime: 30 days.** And crucially, a concurrency cap:

> 开放平台发放的 Access Token 也将作为设备计算。**同一时刻只允许 client_id 下最多 3 个token 正在使用。多次颁发会存在将 Token 踢下线的情况。颁发后的 Token 有效期为 30 天。**
> — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txgcvbfgh0gtuad5>

There is **no refresh_token on the client-credentials path.** You re-`POST /api/v1/access_token` before expiry. (A real refresh token exists only on the OAuth/第三方挂载 path, which is closed to individuals and irrelevant to a server-side storage backend.) Multi-instance deployment means every replica must share one token or you will thrash the 3-token limit and knock instances offline — a real footgun for the "2 replicas behind Caddy" plan.

Tokens are JWTs (`eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...`), so expiry can be read locally, but the docs say to use `expiredAt`.

### Required headers on every call

| Header | Value |
| --- | --- |
| `Authorization` | `Bearer <access_token>` (note: `Bearer` + space + token) |
| `Platform` | `open_platform` (fixed) |
| `Content-Type` | `application/json` |

`Platform` is required even on the token call. The FAQ says slice `PUT` requests need **neither** `Authorization` nor `Platform` (<https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/ghfd4h0l6c6y6oi8>), although the V2 slice doc example sends them.

### Universal response envelope

`{ code, message, data, x-traceID }`; `code == 0` means success. Documented codes: `1` internal error, `401` invalid token, `429` too frequent, `5066` file not found, `5113` **流量超限 / download quota exhausted**.
— <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txgcvbfgh0gtuad5>

---
## (b) Upload APIs

### Max sizes (two different ceilings — easy to get wrong)

| Path | Max single file |
| --- | --- |
| Chunked upload `POST /upload/v2/file/create` | **10 GB** — *"开发者上传单文件大小限制10GB"* |
| Single-step upload `POST {upload_domain}/upload/v2/file/single/create` | **1 GB** — *"此接口限制开发者上传单文件大小为1GB"* |

— <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txow0iqviqsgotfl> and <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/xhiht1uh3yp92pzc>

### Flow

**Small file (≤1 GB) — one request**, after fetching an upload domain:

```
GET  https://open-api.123pan.com/upload/v2/file/domain   -> [ "https://openapi-upload.123242.com", ... ]
POST {upload_domain}/upload/v2/file/single/create
     multipart/form-data: file=<bytes>, parentFileID, filename, etag=<md5 hex>, size, [duplicate], [containDir]
  -> { "fileID": 11522653, "completed": true }
```

**Large file — create → slice → complete** (the real path for video/music):

```
POST /upload/v2/file/create
  { "parentFileID": 0, "filename": "测试文件.mp4",
    "etag": "<md5 hex>", "size": 50650928,
    "duplicate": 1|2, "containDir": false }
  ->
  { "fileID": 0,
    "reuse": false,                         // true => 秒传, upload already done, stop
    "preuploadID": "WvjyUgonimrlBq2PVJ3bSyj...",
    "sliceSize": 16777216,                  // 16 MiB — you MUST chunk at exactly this size
    "servers": [ "http://openapi-upload.123242.com" ] }

POST {servers[n]}/upload/v2/file/slice      // multipart/form-data
  preuploadID, sliceNo (1-based), sliceMD5, slice=<bytes>
  -> data: null (HTTP 200 is the success signal)

POST https://open-api.123pan.com/upload/v2/file/upload_complete
  { "preuploadID": "..." }
  -> { "completed": true, "fileID": 11522654 }
     // if completed == false, poll this endpoint every 1s until it is
```

— create: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txow0iqviqsgotfl> · slice: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/scs8yg89yz8immus> · complete: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/fzzc5o8gok517720> · flow: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/xogi45g7okqk7svr>

### Yes — you must "create file" first, and MD5 is mandatory

`etag` (file MD5) and `size` are **required** on both create and single-step, and `sliceMD5` is required per slice. The MD5 is not optional bookkeeping — it drives **秒传 (instant upload / dedup)**: if the same hash already exists on 123pan servers, `reuse: true` comes back and your bytes are never transferred. Implications:

- **You must hash every attachment before upload.** For a chat app that is a full read pass over every byte (CPU), or a client-supplied hash you cannot trust.
- **Cross-tenant content dedup is inherent**: dedup is by hash platform-wide, so one user's attachment may be "the same file" as another's. `fileList` even exposes `etag`, `s3KeyFlag` and `storageNode` — internal infra detail leaking through the API.

### Filename rules (a compatibility tax)

> 文件名要小于256个字符且不能包含以下任何字符：`"\/:*?|><` … 文件名不能全部是空格 …（注：不能重名）

`duplicate` controls collisions: `1` = keep both (auto-suffix), `2` = overwrite. Name collisions are rejected by default, so the backend must encode/escape names and handle collisions itself. That is why rclone added an encoder — and still ships an unfixed bug:

> 当文件夹名中有特殊字符、符号时会使文件名发送改变，例如"世界计划：无法歌唱的初音未来 (2025)"上传后名称会发送改变"世界计划‛：无法歌唱的初音未来 (2025)"
> — <https://github.com/rclone/rclone/pull/9047>

### Known hard limitations surfaced by the rclone backend

> **Limitations (123Pan API limitation):**
> - No modification time support
> - Empty files (0 bytes) are not supported
> — <https://github.com/rclone/rclone/pull/9047>

**"No modification time support"** is close to disqualifying for message attachments, which are inherently time-ordered: you would have to keep mtime in Postgres and never trust the store. **"0-byte files unsupported"** breaks empty-file uploads outright.

### Quota implications

Uploading itself is not metered ("上传不限制"), but the *download* side is (see (c) and (f)), and total capacity is the plan's capacity (free ~2 TB headline, VIP 20 TB, SVIP 100 TB). You would be spending a **consumer plan's** storage quota to hold production data for all users — no SLA, no capacity guarantee, terminable without notice.

---
## (c) Download APIs — direct URL vs proxy

**You get a direct URL. You do not proxy.** But it is expiring, metered, and not a CDN you control.

```
GET https://open-api.123pan.com/api/v1/file/download_info?fileId=14749954
Headers: Authorization: Bearer <token> ; Platform: open_platform
->
{ "code": 0, "message": "ok",
  "data": { "downloadUrl": "https://download-cdn.cjjd19.com/123-61/ab6dd0cf/18..." },
  "x-traceID": "..." }
```

— <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/fnf60phsushn8ip2>

Documented failure modes on this exact endpoint:

| code | meaning | example message |
| --- | --- | --- |
| `5113` | 自用下载流量不足 | *"您今日自用下载流量已超出1GB上限，升级VIP会员可无限流量下载"* |
| `5066` | 文件不存在 | *"文件不存在，检查传入fileId是否正确"* |

Rate limit on this endpoint: **QPS 5** per uid.

**Expiry:** the official `download_info` page does **not** document a TTL on `downloadUrl`. The URL is served from a CDN host (`download-cdn.cjjd19.com`). The *related but distinct* **直链** (direct-link) product does expose an expiry setting — AList's driver docs describe *"文件直链有效期，单位为分钟，默认填充为30分钟"* (<https://alistgo.com/zh/guide/drivers/123.html>) — but that is the consumer 直链 feature, a different mechanism. **Treat the `download_info` TTL as undocumented and short-lived: re-request per download, never persist these URLs.** (Explicitly flagged as an unknown rather than asserted as 30 min.)

**Hotlinking / embedding** is permitted only via the paid 直链 product, and it is metered traffic:

- 直链 requires VIP: *"使用直链需要开通会员服务。会员有效期内，在每月的会员订购日赠送10GB流量，赠送流量有效期为一个月"*
- VIP 直链流量包 **10 GB/month**, SVIP **100 GB/month** (down from 100 GB / 1 TB in June 2024)
- beyond that you buy 直链流量包; packs bought or point-redeemed are now long-lived ("长期有效，用完为止")
- custom domain + HTTPS is a paid add-on: *"支持额外云盘容量包、自定义域名HTTPS请求包"*

— <https://www.123pan.com/vipcenter> · <https://www.laoliang.net/xiantan/21866.html> · <https://finance.sina.com.cn/tech/digi/2025-12-13/doc-inharzef4525342.shtml>

**The decisive point:** every byte of every attachment your end users fetch hits this meter. For an IM app, that meter *is* the cost model. Compare S3/R2, where egress is metered at a competitive rate under a contract, or (Cloudflare R2) zero.

Also note the API's own `download_info` path counts against **自用下载流量** — the app-owner account's quota — with **free tier 10 GB/month across all clients combined** (PC/web/APP; using the official client halves the charge; overage 0.05 CNY/GB; per-purchase minimum 10 GB = 0.5 CNY). The old no-login free path for <100 MB files was **removed**:

> 【取消权益】游客身份下载小于 100MB 文件免费权益将终止 … 此举旨在封堵"爬虫软件批量抓取或高频下载"的漏洞
> — official notice reproduced at <https://github.com/XiaoHe023/123panYouthMember>

---

## (d) Management APIs

| Operation | Endpoint | Notes |
| --- | --- | --- |
| List (recommended) | `GET /api/v2/file/list?parentFileId=&limit=&lastFileId=&searchData=&searchMode=` | `limit` ≤ 100; `lastFileId == -1` means last page; **result includes trashed files** — filter on `trashed` |
| List (legacy) | `GET /api/v1/file/list` | QPS 0.2 free — effectively unusable |
| File detail (batch) | `POST /api/v1/file/infos` | body `{ fileIds: [...] }` |
| Create directory | `POST /upload/v1/file/mkdir` | QPS 5 free / 20 VIP |
| Move (batch) | `POST /api/v1/file/move` | *"批量移动文件，单级最多支持100个"*; body `{ fileIDs: [], toParentFileID: 0 }` |
| Rename (single) | `PUT /api/v1/file/name` | body `{ fileId, fileName }` |
| Rename (batch) | — | present in the official doc tree |
| Delete → recycle bin | `POST /api/v1/file/delete` | *"删除文件至回收站"*; QPS **1** |
| Hard delete | — | *"彻底删除文件"* |
| Restore | — | *"从回收站恢复文件"*, *"还原文件到指定目录"* |
| Trash list | `GET /api/v1/file/trash` | QPS 5 |
| Download URL | `GET /api/v1/file/download_info` | QPS 5 |

— list: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/zrip9b0ye81zimv4> · move: <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/rsyfsn1gnpgo4m4f> · rename: <https://docs.rs/crate/restic-123pan/latest/source/docs/123pan/API列表/文件管理/重命名/单个文件重命名.md> · complete tree: the verbatim vendored doc book at <https://github.com/gaoyifan/restic-123pan/tree/main/docs/123pan/API列表>

**Organising app files in a directory tree** is supported — `parentFileID`/`parentFileId` is a folder id with `0` as root, and `upload/v2/file/create` accepts `containDir: true` plus a path-prefixed `filename` (e.g. `/你好/123/测试文件.mp4`) so the path is created implicitly on upload. But the sharp edges are significant:

- **Deleting goes to the recycle bin by default.** A naive `delete` leaves the bytes in 123pan's trash — you must issue a second, separate hard-delete to actually free quota. Get this wrong and you silently accumulate paid storage.
- **Batch move caps at 100 per call**; batch rename/delete caps at 100 per the community SDKs (*"批量操作单次最多100个文件"*, <https://github.com/kibble5788/pan123>).
- **No mtime** — a filesystem cannot be mirrored onto it.
- **Listing is slow and includes trash**; pagination is by `lastFileId`, not offset.
- Files are subject to **content moderation**: the file object carries `status` (*"文件审核状态。大于 100 为审核驳回文件"*) and `punishFlag` (*"惩罚标记"*). Files can be administratively rejected — out of your control. For user-uploaded attachments that is an availability risk, and per your own constraint *"秘密聊天不得参与内容审核"* it is an outright contradiction if E2EE blobs ever land here.

---
## (e) Rate limits & concurrency

Two official tables exist and they disagree. The 第三方挂载 (OAuth/uid) table is *"同一个uid，每秒最大请求次数"*; the 开发者 (client_id) table splits 免费用户 / 会员用户. Both reproduced here.

**Per-`uid` (OAuth path)** — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/kf05anzt1r0qnudd>

| API | QPS |
| --- | --- |
| upload/v1/file/create | 5 |
| upload/v1/file/get_upload_url | 20 |
| upload/v1/file/list_upload_parts | 20 |
| upload/v1/file/mkdir | 5 |
| upload/v1/file/upload_async_result | 5 |
| upload/v1/file/upload_complete | 20 |
| api/v1/file/delete | 1 |
| api/v1/access_token | 8 |
| api/v1/file/list | 1 |
| api/v1/file/trash | 5 |
| api/v1/file/move | 10 |
| api/v1/user/info | 10 |
| api/v2/file/list | 15 |
| api/v1/file/infos | 10 |
| api/v1/file/download_info | 5 |
| api/v1/oauth2/access_token | 100 /min (**triggers IP-level limiting**) |

**Per-`client_id` (the path a server backend would use)** — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txgcvbfgh0gtuad5>

| API | 免费用户 QPS | 会员用户 QPS |
| --- | --- | --- |
| api/v1/user/info | 10 | 10 |
| api/v1/file/move | 3 | 10 |
| **api/v1/file/delete** | **1** | 10 |
| **api/v1/file/list** | **0.2** | 10 |
| api/v2/file/list | 5 | 10 |
| upload/v1/file/mkdir | 5 | 20 |
| api/v1/access_token | 8 | 10 |

**Additional concurrency limits:**

- **≤ 3 concurrent `access_token`s per `client_id`**; issuing more kicks existing ones offline (开发须知).
- Account-level device cap surfaces as its own error: *"如果接口提示 `tokens number has exceeded the limit` … 说明您的账号登录数量过多"* (<https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/ghfd4h0l6c6y6oi8>).
- rclone's backend had to implement *"Automatic VIP level detection with adaptive rate limiting"* precisely because limits vary by the account's VIP tier.
- Real-world consequence reported upstream: *"bisync and gitannex are recommend disable, due to QPS limit, it will takes a long time"* (<https://github.com/rclone/rclone/pull/9047>).
- A community client hard-codes a **10-second sleep every 5 pages** to survive list throttling (`RATE_LIMIT_INTERVAL = 10`, `RATE_LIMIT_PAGES = 5`, <https://github.com/Bao-qing/123pan>).

**Verdict:** `delete` at 1/s and `file/list` at 0.2/s mean a chat backend cannot keep metadata in sync with the store at any realistic message rate. These limits are tuned for *a single human's* file manager, not an application — the clearest available signal of intended use.

---
## (f) Terms of service / compliance risk — **this is the blocker**

Sources: 《123云盘开放平台开发者服务协议》 (official URL `https://www.123pan.com/DeveloperAgreement`, JS-rendered; full text verified via mirror <https://www.itanlian.com/fanben/otherht/84451.html>), plus 123云盘's own public anti-abuse statements.

### 1. The agreement's stated service model is *per-end-user accounts*, not "app-owned storage"

> 开放接口服务：是指西安一二三公司根据本协议向开发者提供的网盘数据存储、同步、管理和分享等在线服务的功能。开发者基于开放接口服务开发的应用程序可以为用户提供**在该应用程序内使用用户本人123云盘存储空间**以及进行文件管理的功能。

> 开发者可将123云盘相关功能嵌入到自有的应用程序或硬件端，**开发者的用户可登录123云盘帐号使用123云盘相关功能**

The proposed design — one jiuyue-owned 123pan account holding every user's attachments — is **not the licensed model.** The licensed model requires each end user to hold and log into their own 123pan account, which is architecturally incompatible with a chat backend that must own and serve attachments to recipients who are not logged into 123pan.

### 2. Explicit ban on competing with their business via the API

> 不会使用123云盘开放平台服务**侵犯西安一二三云计算有限公司的商业利益**，包括但不限于利用123云盘开放平台提供的**数据接口开展与西安一二三云计算有限公司存在竞争关系的业务**、或通过123云盘开放平台服务进行任何可能影响123云盘及123云盘的开放平台服务正常运转的行为。

123云盘 **sells** 直链流量 and positions 直链 as *"平替商业 CDN加速"* on their homepage. Using their disk as the free/cheap origin + CDN for our media is, on its face, using the data interface to run a business in competition with that product. **This is the single most dangerous clause for the proposed design.**

### 3. No trademark / branding use without written consent

> 除西安一二三云计算有限公司书面同意外，开发者不得使用…（包括但不限于123云盘等）的商号、商标、服务标志、企业名称、其他标志、标识、用语等开展业务活动或进行任何宣传、盈利或非盈利行为

Practical impact: you may not say "jiuyue stores your files on 123云盘" in marketing or UI without written consent. That removes storage-transparency disclosure to users.

### 4. Termination at will, no notice, no liability — and you waive the right to complain

> **免责声明** — 西安一二三云计算有限公司…**123云盘开放平台可以在任何时候取消或终止开发者的全部或部分服务。取消或终止服务的决定不需要理由或通知您**，或征求您的意愿，一旦服务取消，您使用本服务的权利立即终止。因123云盘变更、中止、终止部分或全部服务而导致开发者的应用程序或服务部分或者全部不可用的，开发者应当自行解决，西安一二三云计算有限公司不承担责任。

> 西安一二三云计算有限公司不保证…123云盘开放平台服务**不受干扰，及时、安全、可靠或不出现错误**

> 开发者承诺对上述收费规则已充分知悉，并予以理解，**不得因123云盘开放平台修改、变更或付费的相关事项提起诉讼、仲裁、投诉、举报及其他权利主张。**

This is the opposite of a storage SLA: no availability commitment, no durability commitment, termination without cause or notice, and a contractual waiver of your right to object to price changes. A foundation that can vanish with no recourse is not a valid primary store for user data.

### 5. Data-protection duties a storage backend violates by construction

> 开发者不得以任何形式将收集的**用户个人信息提供给任第三方**有偿或无偿使用
> 开发者不得收集…不得收集用户的123云盘帐号、密码或身份证号等敏感个人信息
> 必须事先获得用户的同意，且仅应当处理产品运行及功能实现所必需的用户数据…开发者应当制定并公开产品隐私政策

Storing user attachments on a third party's consumer cloud drive without a data-processing agreement, and without user consent covering that transfer, is precisely what this prohibits. No DPA / 数据处理协议 is available on a consumer-cloud basis.

---
### 6. 123云盘 has publicly named this exact pattern as the abuse they are shutting down

From their official 致全体用户书 (2 Dec 2025), quoted by 网易/IT之家:

> 我们发现，有一类用户并非普通使用者，而是**利用平台规则漏洞，进行批量注册、破解接口、盗链建站，甚至搭建影视网站、小文件下载站等商业服务**。他们单个站点消耗的带宽与流量，往往相当于成千上万普通用户的总和。这类行为不仅严重挤占服务器资源，更直接导致正常用户在高峰时段体验下降。我们必须明确：我们打击的对象不是普通免费用户，而是那些**系统性、商业化滥用资源的"羊毛党"**。

And on the media-serving case specifically:

> 这一调整，主要是为了**封堵影视类网站 / App 的盗链行为**。此前，免费用户可无限播放 480P 转码视频，这被部分站点利用：他们**批量注册账号，为自己的访客提供视频在线播放服务**，消耗了我们大量带宽。

And on no-login download (the mechanism a public attachment URL relies on):

> 取消该权益后…这一规则长期被一些**"小文件下载站"**利用，例如字体、素材、小游戏等站点，**通过游客接口为其用户提供免费下载**。

— <https://www.163.com/dy/article/KFQEN5H50511B8LM.html>

**This is a description of the proposed architecture.** "An app that serves files to its own visitors from a 123pan account" is not a grey area here — it is the named target of an active crackdown, enforced by quota cuts (30 GB→10 GB/month), removal of no-login download, counting online media playback against quota, and IP-level blocking.

### 7. Known enforcement behaviour against integrators

- **IP-level blocking of automated access.** netdisk-fast-download reports `123pan-global-slb forbidden client ip` — *"当单个IP地址在短时间内发起过多解析请求时，123云盘的服务器会将该IP列入临时黑名单"* — and recommends private deployment or proxy rotation to evade it (<http://www.rskf.cn/news/1798>).
- **"陌生设备挂载" bans.** 123云盘 forbids mounting from unrecognised devices; free users hitting this must log into the web client or change their password to clear it (<https://alistgo.com/zh/guide/drivers/123.html>). A headless HK server is by definition an unrecognised device.
- **Direct-link anti-hotlink escalation**, including Referer/UA checks and tightening token TTLs — the whole "123云盘直链解析" sub-industry exists because the platform actively breaks it (<https://ask.csdn.net/questions/8768382>).
- Community SDKs carry explicit ban-risk disclaimers: *"使用本项目所造成的一切后果，包括但不限于数据丢失、**账号封禁**等，均由使用者自行承担"* (<https://github.com/xxxxxfpx/123PanOpen>). The AList `123` driver is marked *"由于123网盘的限制，此驱动不再积极维护"*.
- A free-user driver is documented as *"禁止多IP共享使用"* — an explicit prohibition on exactly the multi-node/shared-account pattern a backend deployment implies.

The agreement permits the platform to *"永久冻结账号…清空与该账号相关联的部分或全部资产…删除或冻结链接"* with no compensation. **If your attachment account is frozen, every attachment in your product is gone at once.**

### (f) verdict

**Using 123pan as jiuyue's storage backend is not permitted by their terms, contradicts their stated service model, competes with their paid CDN product, and matches the abuse pattern they are actively enforcing against.** The risk is not "might get rate-limited" — it is **irreversible, total data loss for all users, with no SLA, no notice, and no recourse**, on top of a contractual waiver of your right to object.

**Do not ship this. Not even as a secondary/backup `Storage` impl** — a backup target that can be terminated without notice, and whose deletion semantics silently route to a recycle bin, is worse than no backup.

---
## (g) SDKs and real-world usage

### Official SDKs

**None.** 123云盘 ships no first-party SDK in any language, and there is **no Rust SDK**. The official docs' "优秀实践" page lists only *user-contributed* demos in Python, NodeJS, PHP and Java:

> 以下Demo皆由热心用户提供…内容仅供参考
> + 简单Python示例 … + 简单NodeJS示例 … + 简单PHP示例 … + 简单Java示例
> — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/gg705bew0t80ccse>

Note the demos are hosted on **Gitee**, not on a 123pan GitHub org — there is no first-party code presence at all.

### Community SDKs (all third-party, all unofficial)

| Library | Language | Notes / maintenance |
| --- | --- | --- |
| `@sharef/123pan-sdk` (<https://github.com/shijf/123pan-api-sdk>) | TypeScript | Active-ish (2025-12); token bucket, auto-retry; *"本 SDK 为非官方实现"* |
| `@ked3/pan123-sdk` (<https://github.com/kibble5788/pan123>) | TypeScript | Enumerates limits: 3 tokens/client_id, 10 GB max file, 100/batch |
| `123pan-go-sdk` (<https://github.com/Qialas/123pan-go-sdk>) | Go | Clean, modular; documents V2 upload + sha1 秒传 + 单步上传 + 直链/图床/分享. Best-documented non-official client. |
| `p123client` (<https://github.com/ChenyangGao/p123client>) | Python | Widely forked. Wraps **web + app + open** APIs. |
| `x123pan` / `123PanOpen` (<https://github.com/xxxxxfpx/123PanOpen>) | Python | auto token mgmt; explicit ban-risk disclaimer |
| `Pan123` (<https://github.com/jonntd/Pan123>) | Python | — |
| `aio123pan` (<https://github.com/Cloxl/aio123pan>) | Python | asyncio, MIT, Dec 2025 |
| `yutao/pan123` | PHP | Packagist |
| **`restic-123pan`** (<https://github.com/gaoyifan/restic-123pan>) | **Rust** | crates.io v0.3.1, last push 2026-08-08, **193 total downloads**. See below. |

**crates.io search result (checked 2026-09-16):** queries `123pan`, `pan123`, `123yunpan`, `pan123-sdk` return exactly **one** crate — `restic-123pan`. **There is no usable Rust 123pan SDK.** You would write the client from scratch.

### The one Rust crate — and why it is *not* evidence for this design

`restic-123pan` is a Rust Axum service exposing the **restic REST API** with 123pan as the store. Its auth module is explicit:

```rust
//! Token management for 123pan **web API** authentication.
pub const BASE_URL: &str = "https://yun.123pan.com";
pub const BAPI_BASE_URL: &str = "https://yun.123pan.com/b/api";
pub const LOGIN_URL: &str = "https://login.123pan.com/api/user/sign_in";
```
— <https://github.com/gaoyifan/restic-123pan/blob/main/src/pan123/auth.rs>

It **logs in with the user's phone/email + password** (`PAN123_USERNAME` / `PAN123_PASSWORD`) against the reverse-engineered **private web API**, sending spoofed browser headers (`platform: web`, `app-version: 3`, fake `origin`/`referer`). It does **not** use the open platform at all. That is the unsanctioned pattern — outside ToS scope entirely, and the same shape of integration the crackdown targets. **It is not a precedent to follow**, and it is the only Rust code in existence for this platform.

### Upstream project integrations

| Project | Status |
| --- | --- |
| **rclone** | **NOT merged.** `https://rclone.org/123pan/` → **404**. PR #9017 closed; PR #9047 was maintainer-approved then **closed by its author on 2026-05-12**, with a reason that matters: *"now the Official do not order the OpenAPI key with unpaid method, so might cannot continue test."* The work lives only in a personal fork (<https://github.com/Higanoneko/rclone>), which ships **two** backends: `123open` (official API) and `123pan` (**web API reverse-engineering**, username/password, "无需开发者凭证"). |
| **restic** | No official support; only the third-party Rust bridge above. |
| **AList / OpenList** | `123 Open` driver exists (<https://alistgo.com/zh/guide/drivers/123_open.html>) and works for mounting; the older `123` driver is marked *"不再积极维护"* due to platform limits. |
| **CloudDrive2, RaiDrive, KODI, Infuse, VidHub, fnOS…** | Listed on 123pan's homepage as authorised 三方应用 — i.e. 123pan's own sanctioned integration direction is **consumer media playback / file mounting**, not app backends. |

**Summary:** zero official SDKs, no Rust, one Rust crate that uses the unsanctioned web API, and the flagship upstream integration (rclone) **failed to merge** because the credential got paywalled. There is **no open-source project using 123pan as a general application storage backend** — because the platform is not built for that and its terms do not allow it.

---
## (h) Reliability and performance

**There is no availability or durability commitment of any kind.** Quoting the developer agreement again, because it is the whole answer:

> 西安一二三云计算有限公司**不保证**…123云盘开放平台服务**不受干扰，及时、安全、可靠或不出现错误**

> 123云盘开放平台**可以在任何时候取消或终止**开发者的全部或部分服务。取消或终止服务的决定**不需要理由或通知您**

No SLA, no uptime target, no status page, no RPO/RTO. For a primary attachment store this alone disqualifies it.

**Documented reliability signals:**

- **Repeated retroactive quota/price changes.** At least 4–5 major adjustments 2023→2026, all restrictive: 2024-06 VIP 直链流量 100 GB→10 GB/month; 2024-09 free users capped at 1 GB/day downloads; 2025-11 free extraction 30 GB→**10 GB/month** with audio/video streaming counted against it; **2026-01-01 price increase** (连续包年 198→258 CNY/yr, +30.3%; 连续包月 18→25 CNY, +38.9%), attributed to inter-carrier settlement costs and hardware prices. — <https://finance.sina.com.cn/tech/digi/2025-12-13/doc-inharzef4525342.shtml>
- **The platform's economics are visibly strained** — official statements about bandwidth cost pressure and PCDN crackdowns, with the free tier being actively shrunk. Using a strained consumer freemium product as production infrastructure inherits that risk.
- **Overseas/IP behaviour — directly relevant to a Hong Kong server.** The rclone author, running from outside China, wrote: *"outside of China's IP might get block to access the API, when that day come, you can ask me for new one"* (<https://github.com/rclone/rclone/pull/9047>). That is first-hand: **a non-mainland IP should expect API blocking.** Independently, parsers report `123pan-global-slb forbidden client ip` and IP blacklisting under automated load, mitigated only by private IPs or proxy rotation (<http://www.rskf.cn/news/1798>). rclone's maintainer could not even register: *"I don't think I can make one without a Chinese mobile phone number - it didn't accept my UK phone number."* **A HK-based backend would run against a service that treats it as foreign and suspicious by default.**
- **Slow transfers and OOM under load.** rclone users on this backend: *"我在使用 rclone copy 进行 123上传的时候出现内存溢出的情况，即使线程只有 1 个，文件数量比较多文件大小在 500MB～70GB"* — upstream had to rewrite non-seekable uploads to disk temp files, cap `maxMemoryUploadSize` at 32 MB, and stream pagination instead of accumulating. Expect to re-solve buffering for video-sized attachments on a 2C2G box.
- **Intermittent upload failures**: *"偶发性错误，有时候能传，有时候就传不了…一直是waiting的情况"*. Plus no 0-byte file support.
- **Speed when it works** is genuinely good from inside China (official claim: 不限速, *"千兆带宽可跑满"*), and one user reported *"transferred around 1TB … almost saturate the upload bandwidth, no throttling so far"* with `--transfers 2`. But that is a China-side, non-production observation, and it is **upload** — the direction that is not metered.

**CDN / direct-link sharing:** a real CDN exists — `download_info` returns `https://download-cdn.cjjd19.com/...`, and 直链 is marketed as *"平替商业CDN加速"* with *"直链CDN预热"*. **But** (i) it is inside China, tuned for Chinese ISP topology — a HK origin is not its design centre; (ii) it is **metered** (10 GB/month with VIP, 100 GB with SVIP, packs beyond); (iii) it is **not appropriate for private chat media**: a shared CDN URL, once leaked, serves the file to anyone. That leak is a live, exploited problem — the 123panYouthMember project exists specifically to abuse *"生成的直链被恶意用户分发给任意普通用户访问"*. For per-recipient authorisation on chat attachments, a public CDN URL is a **privacy regression**, and it contradicts your own rule *"下载鉴权永远在应用层，绝不下发裸文件 URL"*.

---
## Rust integration sketch (for the record)

The verdict is **do not build this**, but here is what preserving the option would cost — use it to price the decision.

```rust
// storage/pan123.rs  —  hypothetical Storage impl
pub struct Pan123Storage {
    http: reqwest::Client,
    base: Url,                          // https://open-api.123pan.com
    creds: Arc<ClientCreds>,            // client_id / client_secret
    token: Arc<RwLock<Option<Token>>>,  // process-local BUT must be shared across replicas
}

struct Token { value: String, expires_at: DateTime<Utc> }
```

**Token lifecycle**

1. `POST /api/v1/access_token` with `Platform: open_platform`, body `{clientID, clientSecret}`.
2. Cache until `expiredAt` minus a safety margin (docs' own guidance: re-fetch *before* expiry). Lifetime is 30 days, so refresh on a timer, not per request — this endpoint is only QPS 8–10.
3. **Persist the token outside the process.** With 2 replicas you are at 2 of the 3-token cap; a container restart or rolling deploy can exceed it and **kick a live instance offline**. Store it in Postgres/Redis with an expiry and treat issuance as a distributed-lock-protected operation. This is a direct consequence of the 3-token rule and it is not optional — without it, restarts cause outages.
4. On `code: 401` → invalidate, re-issue once, retry the original call. On `code: 429` → exponential backoff with jitter (the rclone backend does *"API retry logic with exponential backoff"* for this reason).

**Upload flow** (`Storage::put`)

```
if size <= 1 GiB:
    domain = GET /upload/v2/file/domain
    POST {domain}/upload/v2/file/single/create   (multipart)
        file, parentFileID, filename, etag=md5(bytes), size
    return fileID
else:                                            # up to 10 GiB
    c = POST /upload/v2/file/create {parentFileID, filename, etag, size, duplicate:1}
    if c.reuse: return c.fileID                  # 秒传 — bytes never sent
    for (i, chunk) in bytes.chunks_exact(c.sliceSize).enumerate():
        POST {c.servers[0]}/upload/v2/file/slice (multipart)
            preuploadID, sliceNo=i+1, sliceMD5=md5(chunk), slice
    loop every 1s until upload_complete returns completed == true
        POST /upload/v2/file/upload_complete {preuploadID}
```

Costs you must accept: a full MD5 pass over every attachment before upload; `sliceSize` is server-chosen (16 MiB observed) and must be obeyed exactly; per-slice MD5; and the `completed:false` polling loop.

**Download URL flow** (`Storage::url`)

```
GET /api/v1/file/download_info?fileId={id}   -> { downloadUrl }
```

- TTL is **undocumented upstream** — assume short; mint per request, never persist, never hand to clients long-term.
- This URL is **public and unauthenticated** at the CDN edge. For private chat media that is an unacceptable authorisation model: your own AGENTS.md says *"下载鉴权永远在应用层，绝不下发裸文件 URL"*. **A 123pan CDN URL is exactly the forbidden naked file URL.**

**Failure handling you would have to implement**

| Condition | Handling |
| --- | --- |
| `401` | re-issue token (respect the 3-token cap), retry once |
| `429` | backoff + jitter; per-endpoint token buckets (limits differ per endpoint and per tier) |
| `5066` | treat as missing; reconcile metadata |
| **`5113` 流量超限** | **quota exhausted — downloads are down until you buy more. Requires a billing/quota alarm and a failover path you would not have.** |
| IP blocked (non-mainland origin) | unfixable from the server; needs a mainland relay |
| Account frozen / service terminated | **no recovery path; all attachments unrecoverable** |
| `status > 100` on a file | content-moderation rejection — attachment silently unavailable |
| `tokens number has exceeded the limit` | account device cap; needs human intervention in the 123pan web UI |
| 0-byte file | unsupported — special-case in your own layer |

Every one of these is a failure mode you do not have with local disk or S3, and several are unfixable from your side.

---
## What to do instead

Keep the `Storage` trait. The decision you actually need is *which S3-compatible provider*, not *whether to invent a cloud-drive adapter*.

| Option | Fit for jiuyue on 1× 2C2G HK |
| --- | --- |
| **Local disk + volume** (default) | Correct starting point. Works, zero egress cost, no ToS risk. Bound the disk, alert at 80%. |
| **S3-compatible behind the existing trait** (Cloudflare R2 / MinIO / 腾讯 COS / 阿里 OSS) | The intended future path. **R2 has zero egress fees** — the exact cost that kills 123pan; R2/MinIO/COS all speak S3, so one impl covers all. Presigned URLs give per-object, expiring, revocable access, satisfying *"下载鉴权永远在应用层"* properly. |
| **Self-hosted MinIO on a second cheap node** | S3 semantics without a vendor, bytes under your control. |
| **123pan** | **Rejected.** See (f); also (c) egress metering, (e) rate limits, (h) no SLA. |

If the appeal of 123pan was "cheap/free storage with no ICP filing", note it fails on exactly the axis that matters (egress metering + no SLA + ToS prohibition), while S3-compatible providers solve it with real contracts and a single S3 API you implement once.

---

## Suggested ADR content

- **Decision:** `Storage` ships with `LocalDisk` (default) and `S3Compatible`. 123pan is explicitly **rejected** and must not be implemented — not even as a secondary/backup target.
- **Consequences:** media egress cost becomes a first-class sizing input; the `Storage` trait must expose per-object **expiring, revocable** access (`presigned_get`) rather than a bare public URL; `Storage` must treat `mtime` as application-owned metadata (recorded in Postgres) because object stores cannot be trusted to preserve it.
- **Rejected alternatives (record the reasoning, not just the name):**
  1. **123pan 开放平台** — terms prohibit the app-owned-account model and competing with their CDN product; egress metered at 10 GB/month free; `download_info` returns public CDN URLs; `list`/`delete` QPS 0.2/1; 3-token concurrency cap; 0-byte files unsupported; no mtime; termination without notice plus a contractual waiver of the right to object.
  2. **123pan WebDAV / web-API reverse engineering** (the `restic-123pan` and rclone-fork approach) — outside ToS entirely, no supported auth, subject to device/IP blocking. Not a basis for a product.
- **Revisit trigger:** only if 123pan publishes a commercial storage/S3 product with a signed SLA, an ICP-compatible commercial contract, unmetered or competitively-priced egress, and an explicit clause permitting the app-owned-account model. None of that exists today.

---

## Sources

**Official (123云盘)**
- API documentation book — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced>
- 接入流程 / 购买开发者权益包 — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/hpengmyg32blkbg8>
- 获取 access_token — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/gn1nai4x0v0ry9ki>
- 开发须知 (30-day token, 3-token cap, QPS table, code table) — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/txgcvbfgh0gtuad5>
- 第三方挂载应用接入 / 授权须知 (enterprise-only, ICP+licence, QPS table) — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/kf05anzt1r0qnudd>
- 常见问题 — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/ghfd4h0l6c6y6oi8>
- 上传 V2 — 创建文件 `/txow0iqviqsgotfl` · 上传分片 `/scs8yg89yz8immus` · 上传完毕 `/fzzc5o8gok517720` · 单步上传 `/xhiht1uh3yp92pzc` · 上传流程说明 `/xogi45g7okqk7svr` (all under `cr6ced`)
- 下载 download_info — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/fnf60phsushn8ip2>
- 获取文件列表（推荐）— <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/zrip9b0ye81zimv4>
- 移动 — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/rsyfsn1gnpgo4m4f>
- 优秀实践 (community demo list) — <https://123yunpan.yuque.com/org-wiki-123yunpan-muaork/cr6ced/gg705bew0t80ccse>
- 开放平台开发者服务协议 — <https://www.123pan.com/DeveloperAgreement> (full text verified via mirror: <https://www.itanlian.com/fanben/otherht/84451.html>)
- Open platform / developer portal — <https://www.123pan.com/open> · <https://www.123pan.com/developer> · <https://www.123pan.cn/developer>
- VIP centre (开发者权益包, 直链, 自定义域名, CDN 预热) — <https://www.123pan.com/vipcenter>
- Direct-link console — <https://www.123pan.com/DirectLink>
- Official statement on abuse / free-tier changes — <https://www.163.com/dy/article/KFQEN5H50511B8LM.html> · notice text reproduced at <https://github.com/XiaoHe023/123panYouthMember>
- Price increase 2026-01-01 — <https://finance.sina.com.cn/tech/digi/2025-12-13/doc-inharzef4525342.shtml>
- 直链流量包 reduction 2024 — <https://www.laoliang.net/xiantan/21866.html>

**Upstream / third-party evidence**
- rclone PR #9047 (closed, not merged; paywall + overseas-IP-blocking comments) — <https://github.com/rclone/rclone/pull/9047> · PR #9017 · issue #9015 · `https://rclone.org/123pan/` → 404
- rclone fork with `123pan` (web API) + `123open` backends — <https://github.com/Higanoneko/rclone>
- `restic-123pan` (Rust; uses web API, not open platform) — <https://github.com/gaoyifan/restic-123pan> · vendored official doc tree at <https://docs.rs/crate/restic-123pan/latest/source/docs/123pan/API列表/>
- crates.io search (`123pan`, `pan123`, `123yunpan`) — one crate only, checked 2026-09-16
- AList `123 Open` driver — <https://alistgo.com/zh/guide/drivers/123_open.html> · `123` driver ("不再积极维护", 陌生设备挂载 ban, 禁止多IP共享) — <https://alistgo.com/zh/guide/drivers/123.html>
- OpenList issue #49 (dev application: open → now audited) — <https://github.com/OpenListTeam/OpenList/issues/49>
- go-123pan-pic (paid-credential requirement, `无效的登录信息`) — <https://github.com/sakuradairong/go-123pan-pic>
- 123pan IP-blacklist behaviour, automated-access blocking — <http://www.rskf.cn/news/1798>
- 123云盘 WebDAV (server host, app passwords, big-file caveats) — <https://zxiaolin.com/798.html> · <https://channel.cx.ms/posts/5507>
- Direct-link anti-hotlink / TTL tightening — <https://ask.csdn.net/questions/8768382>

### Open questions (not resolved by available sources)

1. `downloadUrl` TTL from `download_info` is **not documented**. Do not assume 30 min — that figure is the consumer 直链 setting. If anyone ever tests it, record the result; the verdict does not depend on it.
2. The exact price of 开发者权益包 sits behind a login-walled 会员中心 (`/vipcenter`); not obtainable without an account.
3. Whether 123pan would grant a commercial storage contract on application to `bd@123pan.com`. The published terms expose no commercial-storage product and require an ICP filing + business licence for the third-party path, so this is a long shot, not a plan.