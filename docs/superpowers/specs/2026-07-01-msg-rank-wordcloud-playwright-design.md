# msg_rank 词云 Playwright 截图迁移设计

## 背景与问题

当前 `msg_rank` 插件的词云功能通过外部 CLI 工具 `wordcloud_cli`（Python `wordcloud` 包）生成图片：

- Rust 代码拼接分词后的文本；
- 通过 `tokio::process::Command` 调用 `wordcloud_cli`；
- 从子进程 stdout 读取 PNG 字节；
- 需要额外维护 Python 环境、字体、mask、停用词等配置。

这引入了 Python 技术栈，与项目主体（Rust + Playwright）不一致，增加了部署和运维成本。

## 目标

1. 移除 `wordcloud_cli` 外部依赖，统一使用项目已有的 Playwright 截图基础设施。
2. 保留现有核心行为：分词、停用词过滤、自定义字体、定时发送词云。
3. 不破坏用户侧配置文件的兼容性（至少保留一个平滑迁移期）。
4. 输出图片尺寸、风格与旧实现接近，避免用户感知明显差异。

## 当前实现概要

- 入口：`plugins/msg_rank/src/word_cloud.rs`
- 配置项：`wordcloud_cli_path`（`plugins/msg_rank/src/config.rs`）
- 分词：`jieba-rs`
- 调用：
  - `tokio::process::Command::new(wordcloud_cli)`
  - 参数：`--background white`、`--mask`、`--stopwords`、`--fontfile`
  - stdin 输入分词文本，stdout 输出 PNG
- 并发控制：进程池 `WORDCLOUD_POOL`（上限 2，等待 5s）
- 发送：`send_word_cloud` 生成 base64 图片消息

项目已有 `utils::screenshot(html, selector)`（`plugins/utils/src/screen_shot.rs`），通过 Playwright 将 HTML 渲染为 PNG，且已被 `msg_rank` 的“每日发言排行”功能使用。

## 候选方案

### 方案 A：wordcloud2.js + HTML 模板 + Playwright 截图（推荐）

将词频数据渲染为 HTML + CSS + JS，使用 [wordcloud2.js](https://github.com/timdream/wordcloud2.js) 在 `<canvas>` 上绘制词云，再调用 `utils::screenshot` 截图。

**优点：**
- 完全复用项目已有的 Playwright 与 `utils::screenshot` 资源池。
- 前端词云库成熟，支持字体、颜色、权重、旋转、背景等配置。
- 与“每日发言排行”现有模板/截图流程一致，维护成本低。
- 移除 Python CLI 依赖，减少技术栈。

**缺点：**
- 需要内嵌或加载 `wordcloud2.js`（约 20KB minified）。
- 对复杂的图片 mask 支持不如 Python `wordcloud` 原生；本次设计暂以简单形状/背景为主，复杂 mask 可作为后续增强。

### 方案 B：纯 Rust 绘制词云

使用 `image` 等 crate 手动在 Rust 中实现词云布局算法。

**优点：**
- 无浏览器/JS 依赖。

**缺点：**
- 词云布局算法复杂（螺旋放置、碰撞检测、mask、字体渲染），开发成本高。
- 功能（颜色映射、旋转、字体）很难达到前端库水平。
- 项目已有 Playwright，没有必要重复造轮子。

### 方案 C：ECharts WordCloud Extension

使用 ECharts + `echarts-wordcloud` 扩展绘制词云。

**优点：**
- ECharts 生态成熟。

**缺点：**
- 需要同时引入 ECharts 核心和 wordcloud 扩展，体积更大。
- 配置相对复杂，对截图场景不如 `wordcloud2.js` 轻量。

## 推荐方案

采用**方案 A：wordcloud2.js + HTML 模板 + Playwright 截图**。

## 实现设计

### 1. 词频统计

保留 `jieba-rs` 分词，在 Rust 侧完成：

1. 从数据库读取时间范围内的消息文本。
2. 使用 `JIEBA.cut` 分词。
3. 过滤单字词、停用词（读取 `stopword.txt`）。
4. 统计词频，取 Top N（如 150）。

输出结构示例：

```rust
struct WordCloudItem {
    word: String,
    weight: u32,
}
```

### 2. HTML 模板

新增 `plugins/msg_rank/templates/wordcloud.html`，基于 `askama`：

- 内嵌 `wordcloud2.js`（避免网络依赖，保证离线截图稳定）。
- 接收 `title`、`words`（JSON 数组）。
- 在 `<canvas id="word-cloud">` 上调用 `WordCloud`。
- 支持自定义字体：如果数据目录下存在 `font.otf`，通过 `@font-face` 以 base64 data URL 注入；否则使用系统默认中文字体栈。
- 固定画布尺寸（如 900×600），背景色可配置，默认白色。
- 通过 JS 在绘制完成后设置 `document.body.setAttribute('data-ready', 'true')`，供 Playwright 等待（如需要）。

### 3. 截图

复用 `utils::screenshot(html.into(), Some("#word-cloud".into()))`，直接截取 canvas 元素。

### 4. 配置兼容

- 新增可选配置项 `wordcloud_background`（默认 `"white"`）。
- 保留 `wordcloud_cli_path` 字段但标记为废弃；新实现不再读取它。
- 若旧配置中 `wordcloud_cli_path` 存在，忽略即可，不报错。

### 5. 并发与资源池

- 移除 `WORDCLOUD_POOL`（子进程池）。
- Playwright BrowserContext 池已在 `utils::screenshot` 中管理，无需额外控制。
- 保留 cron 逻辑不变。

### 6. 错误处理

- 词云为空：返回空图片，上层走“词云为空”路径。
- 截图失败：沿用现有“私聊 admin 通知”路径。

## 文件变更

| 文件 | 变更 |
|------|------|
| `plugins/msg_rank/src/word_cloud.rs` | 重写 `make_word_cloud`：移除 CLI 调用，改为生成 HTML + `utils::screenshot`；保留 `send_word_cloud` 与命令处理。 |
| `plugins/msg_rank/src/config.rs` | 废弃 `wordcloud_cli_path`，新增可选 `wordcloud_background`。 |
| `plugins/msg_rank/templates/wordcloud.html` | 新增 askama 模板，内嵌 wordcloud2.js。 |
| `plugins/msg_rank/Cargo.toml` | 无需新增依赖，已依赖 `utils`；确认保留 `jieba-rs`。 |

## 回退策略

- 工作于独立分支 `feature/playwright-wordcloud`，不影响 `master`。
- 若 Playwright 截图不稳定或效果不及预期，可直接丢弃分支或保留 CLI 路径作为 fallback（不在本次实现）。

## 验证方式

1. `cargo build` 通过。
2. `cargo test` 通过。
3. 手动触发 `/wordcloud` 命令（或临时调用内部函数）验证图片生成。
4. 对比新旧词云图片：尺寸、可读性、中文字体渲染正常。
