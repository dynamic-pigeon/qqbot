# QQ Bot

基于 Kovi 和 OneBot 的 Rust QQ 机器人。群内 `/help` 看命令列表，`/help <命令>` 看用法。

| 插件 | 作用 | 入口 |
|---|---|---|
| `markdown` | Markdown 渲染成图片 | `!md` |
| `msg_rank` | 发言排行、词云、群活跃周报 | `/B话榜`、`/wordcloud`、`/周报`、`/消息采集` |
| `bilibili` | B 站直播与动态订阅 | `/live`、`/dynamic` |
| `yu_gi_oh` | 游戏王查卡 | `/查卡` |
| `wordle` | 群内英文 Wordle（公开局，全群共猜） | `/wordle` |
| `image_lib` | 按群图库 | `图库`、`添加`、`来只` |
| `help_msg` | 命令帮助 | `/help` |

## 运行

```bash
cp .env.example .env
cp config.toml.example config.toml
cargo run --release
```

没有 `kovi.conf.toml` 时会交互生成。日志按天写到 `logs/`，保留 30 个文件。

Linux 部署加上分配器 feature。`jemalloc` 与 `mimalloc` 互斥，同时打开会编译失败。musl 默认分配器几乎不归还词云、截图留下的空闲页，RSS 会停在高水位。

```bash
cargo run --release --features jemalloc
# 或
cargo run --release --features mimalloc
```

图库发图默认把图片以 base64 嵌进消息。加上 `image-send-file` 改为 `file://` 路径直发，OneBot 端要能读到 `data/image_lib/`。

```bash
cargo run --release --features jemalloc,image-send-file
```

`image_lib` 的裁剪检测动态链接系统 OpenCV，目标机要有相同 minor 版本的运行库。动态链接 OpenCV 后使用 glibc 目标；交叉部署用 cargo-zigbuild，按目标机的 glibc 版本钉版编译。

## 配置

| 文件 | 用途 |
|---|---|
| `config.toml` | 进程级静态配置，模板是 `config.toml.example`。段内未知键在启动时失败 |
| `kovi.conf.toml` | OneBot 连接与机器人管理员 |
| `kovi.plugin.toml` | 插件启用与访问控制 |
| `.env` | `BILIBILI_COOKIE`、`BILIBILI_USER_AGENT`、`PRIVATE_NETWORK_PROTECTION` |

这四个文件不提交。运行时状态写在各插件的 `data/` 下。

Unix 上会把 `.env`、`kovi.conf.toml`、`config.toml`、插件 `config.json`、消息库和图库 sqlite 权限收紧为 `0600`。

Markdown 截图和发言排行截图需要本机 Chrome 或 Chromium。

B 站动态订阅默认游客身份。普通 HTTP 被风控时改用本机 Chrome。要覆盖游客会话或 UA，按 `.env.example` 填写 `BILIBILI_COOKIE`、`BILIBILI_USER_AGENT`。

图片下载的私网保护默认关闭，以便流量经本地代理转发。在 `config.toml` 的 `[network] private_network_protection` 打开，或设 `PRIVATE_NETWORK_PROTECTION=true`。环境变量优先于配置文件。该开关只覆盖走 `utils` 图片下载的路径。

图片文字识别走腾讯云。`[ocr]` 的 `secret_id` 和 `secret_key` 都填写后才启用，否则跳过。每条消息最多识别 3 张图。

## 数据

### 消息、排行、词云、周报

管理员执行 `/消息采集 enable` 后，该群消息才入库。`/wordcloud enable` 和 `/周报 enable` 也会打开采集。`/wordcloud disable` 只停定时词云，`/周报 disable` 只停定时推送，采集继续。单条最多入库 4 KiB。保留天数、排行人数、词云定时和并发见 `config.toml` 的 `[msg_rank]`。

`/B话榜` 出发言排行，档位为今日、昨日、前日、本周、上周、本月。`/周报` 出上周报告，含发言榜、全勤、每日消息量、夜聊王、早起王和热词。推送 cron 与榜单人数见 `[msg_rank]`。夜聊按本地 23:00–05:59 统计，早起按 06:00–08:59，时段内至少 3 条消息才参与加冕。

中文词云字体放在 `data/msg_rank/font.otf`。没有该文件时使用 wordcloud-rs 内嵌英文字体。可选遮罩为同目录的 `mask.png`、`mask.jpg` 或 `mask.jpeg`。

### 图库

容量、单图上限和抽图限流见 `[image_lib]`。数据在 `data/image_lib/`。

备份与对账由 `backup_cron` 触发（默认 `0 4 * * *`，本地时区）。快照在 `data/image_lib/backups/<日期>/`，保留七天。删除只清索引，文件在备份保护期结束后由对账统一回收，磁盘释放最多滞后七天。保护期内可按 `plugins/image_lib/src/store/backup.rs` 模块注释里的步骤把快照拷回恢复。

配 `cloud_sync_destination`（rclone 的 `remote:path`，服务器需装有 rclone）后，每日备份完成会把图库根目录镜像到该远端，不配即关闭；正式库的 `index.db` 不上传，云端恢复以快照为准。图库超过 `cloud_sync_max_gib`（缺省 9 GiB）时跳过同步并报错。

管理员「查重 <库名>」扫近重复，可用「查重 <库名> 90%」按百分比覆盖阈值。「查裁剪 <库名>」用 SIFT 扫裁剪局部对，每组先整体后局部；补算检出新对时，本轮新增的对会单独发一份。指纹与特征懒计算并落库，首次全库扫描较慢。

「查裁剪」的两两配对也持久缓存：命中的对与已比对集合落库，再次查询直接出结果。库变更后只补算新增图片的对。删图不触发重算；对应 blob 过保护期被回收后，同内容图片再加入才增量重算它的对。修改 `duplicate_distance` 或升级算法会整库弃账重算。

### Wordle

词库在首次使用时下载到 `data/wordle/`，也可预先放好 `answers.txt` 和 `allowed.txt`。对局结束时展示答案的中文释义，2315 个标准答案词已内置。自定义词库可另放 `meanings.txt`，每行 `word<TAB>释义`，用来覆盖或补充释义。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo audit
```

workspace 测试会带上 `utils` 的 `chromium`、`screenshot`、`markdown`，以及 Wordle 的 `qq`。单独测 `utils` 时加上 `--features markdown`，否则 Markdown 与截图测试不会编进来。编译 `image_lib` 需要系统 OpenCV，以及系统 libclang（`libclang-dev`）。

依赖公网 API 或本机 Chrome 的测试标了 `ignored`：

```bash
cargo test --workspace -- --ignored
```
