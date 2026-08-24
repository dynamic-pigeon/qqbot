# QQ Bot

基于 Kovi 和 OneBot 的 Rust QQ 机器人。插件：Markdown 截图、发言排行与词云、B 站直播/动态订阅、游戏王查卡、英文 Wordle、按群图库。

## 运行

```bash
cp .env.example .env
cp config.toml.example config.toml
cargo run --release
```

| 文件 | 用途 |
|---|---|
| `config.toml` | 进程级静态配置，模板是 `config.toml.example` |
| `kovi.conf.toml` | OneBot 连接 |
| `kovi.plugin.toml` | 插件启用与访问控制 |
| `.env` | `BILIBILI_COOKIE` 等环境变量 |

`config.toml`、`kovi.conf.toml`、`kovi.plugin.toml`、`.env` 不提交。

Markdown 截图和发言排行截图需要本机 Chrome/Chromium。B 站动态订阅默认游客身份，普通 HTTP 被风控时会用本机 Chrome 后备。

图片下载的私网保护默认关闭，可在 `config.toml` 的 `[network] private_network_protection` 打开，或设 `PRIVATE_NETWORK_PROTECTION=true`（环境变量优先）。该开关只影响走 `utils` 图片下载的路径。

Unix 上会把 `.env`、`kovi.conf.toml`、`config.toml`、插件 `config.json`、消息库和图库 sqlite 权限收紧为 `0600`。

## 数据

- `/wordcloud enable` 之后该群消息才入库；`/wordcloud disable` 停止采集。单条最多 4 KiB。保留天数、词云定时和并发见 `config.toml` 的 `[msg_rank]`。
- 图片 OCR 每条最多 3 张，需在 `[ocr]` 填写腾讯云密钥，否则跳过。
- 中文词云字体：`data/msg_rank/font.otf`；没有则用 wordcloud-rs 内嵌英文字体。
- Wordle 词库首次使用时下载到 `data/wordle/`，也可预放 `answers.txt` / `allowed.txt`。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p utils --features markdown --all-targets --locked -- -D warnings
cargo clippy -p wordle --all-features --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test -p utils --features markdown --locked
cargo test -p wordle --all-features --locked
cargo audit
```

`utils` 的 `markdown` / `screenshot` 与 Wordle 的 `cli` 不在默认 feature 里。单独检查这两个 crate 时要用上面的 `-p` 命令把对应模块编进来。Wordle 独立 CLI：`cargo run -p wordle --features cli`。

依赖公网 API 或本机 Chrome 的测试标了 `ignored`：

```bash
cargo test --workspace -- --ignored
```
