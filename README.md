# QQ Bot

基于 Kovi 和 OneBot 的 Rust QQ 机器人。插件：Markdown 截图、发言排行与词云、B 站直播/动态订阅、游戏王查卡、英文 Wordle、按群图库。群内 `/help` 查看命令。

## 运行

```bash
cp .env.example .env
cp config.toml.example config.toml
cargo run --release
```

没有 `kovi.conf.toml` 时会交互生成。Linux musl 部署请加 `--features jemalloc` 或 `mimalloc`（二者互斥），否则词云/截图后 RSS 不易回落。

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
- 图片 OCR 每条最多 3 张，需在 `[ocr]` 填写腾讯云密钥，否则跳过。
- 中文词云字体：`data/msg_rank/font.otf`；没有则用 wordcloud-rs 内嵌英文字体。可选遮罩同目录 `mask.png` / `mask.jpg`。
- 图库容量、单图上限和抽图限流见 `[image_lib]`；数据在 `data/image_lib/`。
- Wordle 词库首次使用时下载到 `data/wordle/`，也可预放 `answers.txt` / `allowed.txt`。结束时展示答案的中文释义（2315 个标准答案词已内置）；自定义词库可放可选的 `meanings.txt`（每行 `word<TAB>释义`）覆盖或补充释义。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo audit
```

workspace 会通过依赖编进 `utils` 的 `chromium` / `screenshot` / `markdown` 和 Wordle 的 `qq`。Wordle 独立 CLI：`cargo run -p wordle --features cli`。单独测 `utils` 时要加 `--features markdown`，否则 Markdown/截图测试不会编进来。

依赖公网 API 或本机 Chrome 的测试标了 `ignored`：

```bash
cargo test --workspace -- --ignored
```
