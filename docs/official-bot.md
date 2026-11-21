# 官方机器人迁移规格

状态：调研完成，待沙箱验证（P0），未动工。

## 1. 目标 / 非目标

目标：

- 全量迁移到 QQ 官方机器人 API（q.qq.com 开放平台）。
- 以 cargo feature 双栈共存：默认构建走 kovi-onebot，`official` feature 切官方驱动，随时可回退。
- 自研 in-repo 驱动 crate，不引入 Node 桥接等额外运行时。

非目标：

- 不做频道（guild）场景，只做 QQ 群 + 单聊。
- 不迁移历史数据：openid 体系与 QQ 号无法关联，旧库封存只读。
- 不动 tele-bot。

## 2. 调研结论摘要

kovi 侧：

- 上游（ThriceCola/Kovi）只有 kovi-onebot / kovi-milky 两个驱动，无官方 API 驱动，无公开计划。
- kovi 0.13 核心只定义 `Driver` trait（`event_channel` + `api_handler` + `message_event_register`），事件类型、reply、发送 API 全在驱动 crate 内定义——自研驱动在架构上成立。
- 本仓库 8 个插件深度耦合 kovi-onebot 类型（`MsgEvent` 的 i64 `user_id`/`group_id`、`OnebotTrait`、`RepliableEvent`）。

官方 API 侧（2026-11 文档口径）：

| 项 | 数值 |
|---|---|
| 群被动消息窗口 | 60 分钟，每条消息可回 5 次（开发者为群主/管理员 50 次） |
| 群主动消息 | 需平台单独授权（实测默认无权限，HTTP 400 `err_code 40034105`）；授权后 1000 条/群/日、单群 20 条/分钟、认证主体 bot 维度 60 条/分钟 |
| 全量群消息 | `GROUP_MESSAGE_CREATE`，管理端开「接收所有消息」 |
| 事件自带身份 | `username` / `member_nickname` / `avatar` / `member_role` |
| 收图 | `attachments[].url` 直链 |
| 富媒体发送 | URL 代下载或 file_data 二进制（≤5MB，图片软限 20MB） |
| 单聊 | 被动 60 分钟 4 次；主动 1000 条/用户/日（前提：用户与 bot 有过互动） |

平台层硬缺失（任何适配层变不出来）：

1. 无 QQ 号 / 群号，只有 openid。
2. 无合并转发、无伪造转发节点。
3. 无 `get_group_member_info` 等价物。
4. 单聊主动消息依赖用户先互动且用户可关闭。

## 3. 功能形态

```
Cargo.toml
  [features]
  official = ["dep:kovi-official"]   # 与 onebot 栈编译互斥
plugins/kovi-official/               # 新 workspace 成员：官方 API 驱动
```

- `official` 开启时 `main` 装配 `OfficialDriver`（`plugins/kovi-official`，已落地）。kovi-onebot 仍作为事件类型层保留——插件 import 不变，互斥只发生在驱动装配层，无需编译守卫。
- 驱动 crate 结构参照 kovi-onebot：`Driver` 实现 + 事件类型 + 发送 API trait + `EventRegistrar`，对上暴露尽量同形的 `on_msg` / `on_group_msg` / `reply` / `send_group_msg` 接口，压缩插件层改动。
- 凭据进 `kovi.conf.toml` 新增 `[official]` 段（`app_id` / `app_secret` / `sandbox`），与 onebot 连接段并存，feature 决定装配哪个驱动；白名单 gitignore 天然忽略该文件，启动 0600 收紧已覆盖。缺段/缺字段启动即报错。
- 注意 `kovi.conf.toml` 里 kovi 核心的 `main_admin` / `admins` 在 official 栈下装的是 openid（§5 管理员登记制），两种构建同段不同身份。

## 4. 驱动设计

连接与鉴权：

- WebSocket 网关接入，access_token 定时刷新，断线重连，心跳保活。
- 事件流：`GROUP_AT_MESSAGE_CREATE`、`GROUP_MESSAGE_CREATE`、`C2C_MESSAGE_CREATE`。

身份模型（已落地）：

- openid 字符串自始至终贯穿：事件类型、发送 action、成员缓存、被动路由、插件存储全部是字符串键。OneBot 栈对应位置是 QQ 号 / 群号十进制串，同一套 `utils::bot` 门面。
- msg_rank 消息库与图库分片目录在旧 INTEGER 数据上做 TEXT 迁移（CAST 成十进制串，历史数据无缝续用）。

出站语义：

- `reply` 携带 msg_id（被动），同一 msg_id 内 msg_seq 自增；没有引用目标的发送（命令回执走 `send_group_message`、定时推送）自动挂该会话最近一条入站消息的槽位，槽位耗尽或 60 分钟窗口外只能裸发，无主动权限即被平台拒绝（40034105）。
- 裸发（主动）在驱动内做 20 条/分钟/群 的令牌桶节流。
- 图片：词云/周报等本地 PNG 走 file_data 直传；远程 URL（B 站封面、卡图）走 URL 代下载；file_info 按 ttl 缓存复用。
- 合并转发 action 不实现，调用返回显式错误。

## 5. 各插件改造点

| 插件 | 判定 | 改造 |
|---|---|---|
| help_msg / wordle / yu_gi_oh / markdown | 基本无痛 | 身份键换哈希 id；`!md` 维持 PNG 渲染 |
| msg_rank | 可活 | 采集器顺带存 `avatar`/`username`（事件自带）；头像首见即落盘、按时间窗定时清扫（榜上必有发言者，缓存必命中）；历史库封存；`notify_admin` 私聊上报降级为群内 @ 或外部渠道（见 §8-8） |
| bilibili | 可活 | 推送与词云/周报一样挂被动槽位（群内 60 分钟内有人发言才推得出去），无主动权限兜底；json 小卡片监听以 text 段解析为主，卡片事件实测后定 |
| image_lib | 部分重设计 | 查重/查裁剪展示形态重做（见 §6）；「回复取图」改走引用消息 `msg_elements`（待验证）；blob 目录键换 openid |
| kovi-plugin-cmd | 砍掉 | `official` 构建下不挂载 |

通用：

- `utils::command` 权限判定：管理员从 QQ 号白名单改为 openid 登记制（管理员向 bot 发一条登记命令绑定）。
- 命令前缀、`/help` 分组等交互层不变。

## 6. image_lib 查重/查裁剪展示（合并转发替代）

候选按优先级：

1. 拼长图：命中结果本地拼贴为一张长图（与 `!md` 同管线思路），一次发送，无额度压力。
2. 图文卡片（msg_type 8）：若沙箱验证支持多图，原生体验最接近。
3. 逐条发图 + 节流：兜底，注意被动 5 次上限，超出部分裸发（无主动权限即失败）。

## 7. Markdown 回执与嵌图

官方 2026/04 起对群聊与单聊开放自定义 markdown（无需模板），驱动的发送层统一决策：

- **纯文本回执**（reply / 主动发送）自动按 msg_type=2 发送；平台拒绝时回退纯文本。
- **文 + 图**：合成一条 markdown——公网 URL 图直接嵌 `![img](url)`（平台转存）；本地图先经素材接口（`/v2/media-images` 三步上传）拿 `qqbot-media://` 引用嵌入。素材引用按内容缓存，滑动 90 天有效、单 bot 上限 100 张。
- **嵌不进的图**（素材配额满 / 上传失败）自动转独立图片消息补发；markdown 整条被拒时嵌入图一并转补发。
- **纯图片消息**保持 msg_type=7 media 形态。
- `!md` 维持本地 PNG 渲染（复杂排版），不依赖 markdown 权限。

## 8. 待沙箱验证清单（P0，GO/NO-GO）

已实测（2026-11-20，沙箱）：

- token 接口可用；`expires_in` 以字符串返回且实际有效期约 9 分钟（非文档的 7200 秒），刷新逻辑已按短票适配。
- 全量群消息：开「接收所有消息」后不 @ 也能收到（见下表第 2 项）。
- GROUP_AT_MESSAGE_CREATE 的 `content` 官方自动去除 @机器人前缀；@ 的其他用户在 `mentions` 数组（不含机器人自身）——入站剥离逻辑两种事件形态下均幂等安全。
- 群主动消息默认无权限（2026-11-20 正式环境实测，`err_code 40034105`「主动消息失败, 无权限」）：不带 msg_id 的发送一律被拒。发送层已改为无 reply_to 时自动挂最近入站消息的被动槽位；定时推送（词云/周报/bilibili）只在群内 60 分钟内有人发言时可达。

| # | 项 | 影响 |
|---|---|---|
| 1 | 个人开发者能否开「接收所有消息」 | 不过则 msg_rank 采集、BV 监听死 |
| 2 | `GROUP_MESSAGE_CREATE` 是否全量下发非 @ 消息 | 2026-11-20 沙箱实测通过：不 @ 直接发 `/help` 事件到达 |
| 3 | 引用消息 `msg_elements` 能否取被引用消息的图 | 2026-11-21 正式环境实测通过：被引消息在 `msg_elements` 的 `message_type=103` 元素（文本+图片直链+宽高），驱动转 reply 段（id=ref_msg_idx、text、images 内联），image_lib 回复取图直接读段数据；用户正文在顶层 `content`（文档所述「正文在 msg_elements」与实测不符） |
| 4 | 图文卡片 msg_type 8 能否承载多图 | 查重展示首选替代 |
| 5 | markdown msg_type 2 发送权限 | §7（机制已落地，无权限自动回退纯文本） |
| 6 | `avatar` 字段覆盖率与 URL 时效 | 头像策略：首见即落盘，定时清扫未活跃缓存（URL 时效被本地缓存解耦） |
| 7 | 个人主体沙箱→正式上架的审核开放度 | 能否正式切流 |
| 8 | C2C 主动消息前提实测 | `notify_admin` 去留 |

GO 门槛：1、2 必须通过；3、4 至少一项通过。其余项决定对应功能的取舍，不阻塞整体。

## 9. 阶段划分

- P0 沙箱验证：注册个人机器人 + 测试群，跑 §8 清单。产出 GO/NO-GO。
- P1 驱动骨架（已完成，`plugins/kovi-official`）：WS 网关（接入点探测 / identify / resume / 心跳 / 重连退避）、access_token 刷新、自有事件类型与注册器（openid 字符串身份）、成员缓存、被动槽位路由、富媒体上传与 file_info 缓存、单会话 20/min 节流、多图拆发、markdown 段（msg_type=2，平台拒绝回退纯文本）。22 个单测。
- P2 插件适配（主体已完成）：`utils::bot` 门面（`BotMsgEvent` trait 双栈实现、`on_msg`/`on_group_msg`/`on_private_msg`、主动发送、`member_profile`、`MessageExt` 消息构建），全部 7 个插件脱离 kovi_onebot；msg_rank / bilibili / image_lib 存储与配置换字符串身份键（含存量数据迁移与数字兼容读取）；查重 / 查裁剪在官方栈降级逐图发送；markdown 回执落在 /help、/live list、/dynamic list、/B话榜 用法。双栈 all-targets 编译零错误，默认栈 252 / 官方栈 251 测试全绿。
- P2 插件适配：按 help/采集 → bilibili → wordle/yu_gi_oh/markdown → image_lib 顺序。
- P3 沙箱全功能烟测（对齐 `onebot_smoke` 思路写 `official_smoke`），正式环境切流。

回退：关掉 `official` feature 即回 onebot 栈。两侧数据键不同，不做双写；切流日旧库封存。

## 10. 风险

- 平台政策变动：额度收紧、「接收所有消息」门槛提高。
- 驱动维护成本：官方 API 迭代需跟（本仓库自持驱动，无上游可依赖）。
- 查重/查裁剪展示体验下降（拼长图不如合并转发原生）。
- C2C 上报链路不稳（用户关闭/未互动即失效）。
- openid 按场景隔离：群 member_openid 与单聊 user_openid 不同，跨场景身份不可互用（`notify_admin` 需单独登记 C2C openid）。
