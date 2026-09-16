# IM 领域模型（通用语言）

本文件定义聊天应用的**通用语言**（ubiquitous language）。所有设计文档、规格、工单与代码命名以此为准。

只收录**本项目特有的概念**；通用编程概念（超时、错误类型、工具模式等）不收录。定义描述概念**是什么**，不描述它如何实现。

---

## 身份与账号

**User（用户）**
一个拥有账号的人。
_Avoid_: 账号、会员、客户

**Device（设备）**
一个已登录的客户端实例（浏览器会话、桌面应用，未来可能的移动端）。一个 User 可同时拥有多个 Device。
**注意**：连接状态与同步游标是**按 Device** 的；已读状态是**按 User** 的。

**Contact（联系人）**
出现在某个 User 联系人列表中的另一个 User。
_Avoid_: 好友、朋友

**Block（拉黑）**
一个 User 主动切断与另一个 User 的双向交互（消息、通话、好友请求）。
_Avoid_: 屏蔽、黑名单条目

---

## 会话

**Conversation（会话）**
一组 Participant 之间**有序、持久**的 Message 流。分为 Direct 与 Group 两种。
_Avoid_: 聊天、房间、频道、thread

**Direct Conversation（单聊）**
恰好两个 Participant 的 Conversation。

**Group Conversation（群聊）**
三个及以上 Participant 的 Conversation，带 Role。

**Participant（参与者）**
属于某个 Conversation 的 User。
_Avoid_: 成员、会员

**Role（角色）**
Participant 在 Group Conversation 中的权限等级：owner / admin / member。
_Avoid_: 权限、级别

---

## 消息

**Message（消息）**
发送到 Conversation 的一条内容单元。

**Message ID（消息标识）**
一条 Message 的全局唯一标识。
_Avoid_: 消息编号、自增 ID

**Sequence Number（会话序号，seq）**
每个 Conversation 内部**单调递增**的整数，按到达顺序分配给每条 Message。它是 Conversation 内**排序的唯一权威**，也是断线补洞的依据。
_Avoid_: 时间戳排序、offset、游标

**Client Message ID（客户端消息标识）**
客户端生成的**幂等键**，用于发送去重与乐观发送（本地先渲染、服务端确认后归位）的关联。

**Attachment（附件）**
由 Message 携带的文件、图片或视频。

**Reaction（表情回应）**
Participant 附加在某条 Message 上的单个 emoji。

**Recall（撤回）**
将一条已发出的 Message 对所有 Participant 变为不可见。
_Avoid_: 删除

**Edit（编辑）**
修改一条已发出 Message 的内容，保留可见的编辑痕迹。

**Quote（引用）**
一条 Message 对其所在 Conversation 中另一条 Message 的显式引用。

---

## 已读与投递

**Read Marker（已读标记）**
User 在 Conversation 中的**私有**位置——"我已读到哪条"。它驱动未读数，**不展示给他人**，且**按 User 而非 Device** 记录。
_Avoid_: 已读回执、已读

**Read Receipt（已读回执）**
Participant **公开**的"我已读到哪条"，会展示给其他 Participant。

**Read Marker 与 Read Receipt 是两个不同概念，任何文档与代码中不得混用。**

**Delivery（送达）**
一条 Message 已到达接收者 Device 的状态。送达区别于已读。

**Unread Count（未读数）**
某个 User 的 Read Marker 之后、被计入未读的 Message 数量。

**Sync Cursor（同步游标）**
某个 Device 已消费的流位置，用于断线后补洞。**按 Device** 记录。

---

## 在线与状态

**Presence（在线状态）**
User 当前是否可达：online / away / offline。

**Typing Indicator（正在输入）**
Participant 正在某个 Conversation 中编辑内容、尚未发送的短暂信号。**不持久化**。

---

## 加密模式

**Cloud Conversation（云端会话）**
服务端持有可解密内容并加密存储的 Conversation；支持搜索、多设备漫游与内容审核。除 Secret Chat 外的一切会话都是 Cloud Conversation。

**Secret Chat（秘密聊天）**
一个**端到端加密**的 Direct Conversation：明文只存在于两端 Device 上，单 Device、不漫游、服务端不可搜索、不参与内容审核。
_Avoid_: 加密聊天（有歧义——云端会话在存储层同样是加密的）、E2EE 会话

**Safety Number（安全码）**
双方用于**独立验证** Secret Chat 未遭中间人替换的指纹，以数字与 emoji 两种形式呈现。

---

## 信任与安全

**Report（举报）**
一个 User 对某个 User 或 Message 发起的违规上报，进入管理后台处置流程。

**Moderation（内容审核）**
对 **Cloud Conversation** 内容执行敏感词检测与人工处置的过程。该过程**不适用于 Secret Chat**。
