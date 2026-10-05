# QQ Bot

基于 Kovi 和 OneBot 的 Rust QQ 机器人。插件：Markdown 截图、发言排行与词云、群活跃周报、B 站直播/动态订阅、游戏王查卡、英文 Wordle、按群图库。群内 `/help` 查看命令。

## 运行

```bash
cp .env.example .env
cp config.toml.example config.toml
cargo run --release
```

没有 `kovi.conf.toml` 时会交互生成。Linux 部署请加 `--features jemalloc` 或 `mimalloc`（二者互斥），否则词云/截图后 RSS 不易回落。

`image_lib` 的裁剪检测动态链接 OpenCV 4.6：绑定按 pkg-config 的 `opencv4` 找头文件与库（`.cargo/config.toml` 不入库，用 `PKG_CONFIG_SYSROOT_DIR` / `PKG_CONFIG_PATH` 指向自己的安装即可），产物动态依赖 `libopencv_*.so.406`，目标机要有同版本运行库。动态链接 OpenCV 后 musl 目标不再可用，交叉部署用 cargo-zigbuild 在目标 glibc 版本上钉版编译。

| 文件 | 用途 |
|---|---|
| `config.toml` | 进程级静态配置，模板是 `config.toml.example`；段内未知键会在启动时失败 |
| `kovi.conf.toml` | OneBot 连接与机器人管理员 |
| `kovi.plugin.toml` | 插件启用与访问控制 |
| `.env` | `BILIBILI_COOKIE`、`BILIBILI_USER_AGENT` 等环境变量 |

`config.toml`、`kovi.conf.toml`、`kovi.plugin.toml`、`.env` 不提交。

Markdown 截图和发言排行截图需要本机 Chrome/Chromium。B 站动态订阅默认游客身份，普通 HTTP 被风控时会用本机 Chrome 后备。

图片下载的私网保护默认关闭，可在 `config.toml` 的 `[network] private_network_protection` 打开，或设 `PRIVATE_NETWORK_PROTECTION=true`（环境变量优先）。该开关只影响走 `utils` 图片下载的路径。

Unix 上会把 `.env`、`kovi.conf.toml`、`config.toml`、插件 `config.json`、消息库和图库 sqlite 权限收紧为 `0600`。

## 数据

- 管理员 `/消息采集 enable` 之后该群消息才入库；`/wordcloud enable` 也会开始采集。`/wordcloud disable` 只停定时词云。单条最多 4 KiB。保留天数、排行人数、词云定时和并发见 `config.toml` 的 `[msg_rank]`。
- 周报：`/周报` 出上周报告（发言榜、全勤、每日消息量、夜聊王/早起王、热词）；管理员 `/周报 enable` 开启定时推送（同时开始采集），`disable` 只停推送。推送 cron 与榜单人数见 `[msg_rank]`，夜聊/早起按本地 23:00–06:00 / 06:00–09:00 统计。
- 图片 OCR 每条最多 3 张，需在 `[ocr]` 填写腾讯云密钥，否则跳过。
- 中文词云字体：`data/msg_rank/font.otf`；没有则用 wordcloud-rs 内嵌英文字体。可选遮罩同目录 `mask.png` / `mask.jpg`。
- 图库容量、单图上限和抽图限流见 `[image_lib]`；数据在 `data/image_lib/`。备份与回收由 `backup_cron` 触发（默认 `0 4 * * *`，本地时区），快照存 `data/image_lib/backups/<日期>/`（保留七天）；删图只清索引，文件由对账在七天备份保护期外统一回收，误删保护期内可直接恢复（方法见 `plugins/image_lib/src/store/backup.rs` 模块注释），磁盘回收因此最多滞后七天。管理员「查重 <库名>」扫近重复，「查裁剪 <库名>」扫裁剪局部对（SIFT 特征匹配，每组先整体后局部）；两者的指纹与特征懒计算并持久化，首次全库扫描较慢。「查裁剪」的两两配对结果也持久缓存：正结果对与已比对集合落库，重复查询直接出结果，库变更后只补算新增图片的对；删图零成本（blob 过保护期被对账回收后，同内容图再加回才增量重算它的对），改 `duplicate_distance` 或算法升级会整体弃账重算。
- Wordle 词库首次使用时下载到 `data/wordle/`，也可预放 `answers.txt` / `allowed.txt`。结束时展示答案的中文释义（2315 个标准答案词已内置）；自定义词库可放可选的 `meanings.txt`（每行 `word<TAB>释义`）覆盖或补充释义。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo audit
```

workspace 会通过依赖编进 `utils` 的 `chromium` / `screenshot` / `markdown` 和 Wordle 的 `qq`。单独测 `utils` 时要加 `--features markdown`，否则 Markdown/截图测试不会编进来。`image_lib` 需要「运行」一节所述的 OpenCV 4.6 与系统 libclang（`libclang-dev`）。

依赖公网 API 或本机 Chrome 的测试标了 `ignored`：

```bash
cargo test --workspace -- --ignored
```
