# QQ Bot

基于 Kovi 和 OneBot 的 Rust QQ 机器人，包含 Markdown 截图、发言排行与词云、B 站订阅、游戏王查卡和英文 Wordle 猜词插件。

## 运行

```bash
cp .env.example .env
cargo run --release
```

OneBot 连接配置位于本地 `kovi.conf.toml`，插件启用和访问控制位于 `kovi.plugin.toml`。这两个文件包含部署相关信息，不提交到 Git。

B 站动态订阅默认以游客身份运行，不需要登录或配置 Cookie。模块会自动获取游客标识并生成 WBI 签名；若普通 HTTP 请求持续触发 B 站风控，会自动使用本机 Chrome/Chromium 后备。`BILIBILI_COOKIE` 仅作为可选兼容配置保留。使用动态订阅或 Markdown 截图时，运行环境需要安装 Chrome/Chromium。

图片下载的私网保护默认关闭，以兼容所有流量经本地代理地址转发的环境。在 DNS 能直接返回目标公网地址的部署中，可设置 `PRIVATE_NETWORK_PROTECTION=true`，启用非公网地址拦截和 DNS pinning。无论该开关是否启用，HTTPS、Host 白名单、重定向禁用、超时和响应体大小限制都会保留。

生产环境应保持 OneBot 服务只监听回环地址或受控内网，并为 `access_token` 使用随机长值。程序在 Unix 上启动时会把 `.env`、`kovi.conf.toml`、插件配置和消息数据库权限收紧为 `0600`。

## 数据与访问控制

- 只有机器人管理员执行 `/wordcloud enable` 后，该群消息才会进入排行数据库。
- 单条入库文本最多 4 KiB，最多保留 8 天；图片 OCR 每条消息最多处理 3 张。
- `/wordcloud disable` 会停止继续采集。已有数据将在保留期到期后自动清理。
- 中文词云需在 `data/msg_rank/font.otf` 放置覆盖中文字符的字体；未提供时使用 `wordcloud-rs` 内嵌的英文字体。
- Wordle 词库（官方答案池与可猜池）首次使用时自动下载并缓存到 `data/wordle/`，也可手动放置 `answers.txt` / `allowed.txt` 跳过下载。
- Markdown、OCR、截图和外部图片下载均有输入、并发、超时和响应体大小限制。
- 公网部署前应在 `kovi.plugin.toml` 中启用插件访问控制并配置允许的好友或群组。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo audit --no-fetch
```

依赖公网 API 的测试默认标记为 `ignored`，需要手动联调时运行：

```bash
cargo test --workspace -- --ignored
```
