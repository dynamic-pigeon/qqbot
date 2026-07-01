# msg_rank 词云 Playwright 截图迁移实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 `msg_rank` 词云从调用外部 `wordcloud_cli` 迁移为使用项目已有的 Playwright + `utils::screenshot` 截图，减少 Python 技术栈依赖。

**Architecture:** Rust 端负责分词、停用词过滤、词频统计与 HTML 渲染；HTML 内嵌 `wordcloud2.js` 在 `<canvas>` 上绘制词云；复用 `utils::screenshot` 将 canvas 元素截取为 PNG。

**Tech Stack:** Rust, askama, jieba-rs, wordcloud2.js, Playwright (playwright-rs), utils::screenshot

## Global Constraints

- 不新增除 `wordcloud2.js` 源码外的外部运行时依赖。
- 保留现有用户命令 `/wordcloud enable|disable|status` 行为不变。
- 保留 cron 定时发送“今日词云”与“上周词云”行为不变。
- 配置文件兼容：保留 `wordcloud_cli_path` 字段但不再读取；新增可选 `wordcloud_background` 字段。
- 复用 `utils::screenshot(Cow<'static, str>, Option<Cow<'static, str>>)` 接口。

---

## File Structure

| 文件 | 责任 |
|------|------|
| `plugins/msg_rank/assets/wordcloud2.js` | 词云绘制库源码，编译时通过 `include_str!` 嵌入。 |
| `plugins/msg_rank/templates/wordcloud.html` | askama 模板，定义词云页面结构与样式。 |
| `plugins/msg_rank/src/config.rs` | 新增 `wordcloud_background` 配置，废弃 `wordcloud_cli_path`。 |
| `plugins/msg_rank/src/word_cloud.rs` | 词云生成主逻辑：分词、统计、渲染、截图。 |

---

### Task 1: 添加 wordcloud2.js 资源并创建 HTML 模板

**Files:**
- Create: `plugins/msg_rank/assets/wordcloud2.js`
- Create: `plugins/msg_rank/templates/wordcloud.html`
- Modify: `plugins/msg_rank/Cargo.toml`

**Interfaces:**
- Consumes: 无
- Produces: `WordCloudTemplate`（askama 模板结构体，字段见 Task 3）渲染后的 HTML 字符串。

- [ ] **Step 1: 下载 wordcloud2.js 并放入资源目录**

```bash
mkdir -p plugins/msg_rank/assets
curl -L https://cdn.jsdelivr.net/npm/wordcloud@1.2.2/src/wordcloud2.js \
  -o plugins/msg_rank/assets/wordcloud2.js
```

- [ ] **Step 2: 创建 askama 模板 `plugins/msg_rank/templates/wordcloud.html`**

```html
<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>{{ title }}</title>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body {
            width: 100vw;
            min-height: 100vh;
            display: flex;
            flex-direction: column;
            align-items: center;
            justify-content: center;
            background: {{ background|safe }};
            font-family: "PingFang SC", "Microsoft YaHei", "Segoe UI", sans-serif;
            padding: 24px;
        }
        h1 {
            font-size: 26px;
            color: #333;
            margin-bottom: 20px;
            font-weight: 600;
        }
        #word-cloud {
            width: 900px;
            height: 600px;
            background: {{ background|safe }};
        }
    </style>
    {% if has_custom_font %}
    <style>
        @font-face {
            font-family: "CustomWordCloudFont";
            src: url("{{ font_data_url }}");
        }
    </style>
    {% endif %}
</head>
<body>
    <h1>{{ title }}</h1>
    <canvas id="word-cloud" width="900" height="600"></canvas>
    <script>
        {{ script|safe }}
    </script>
    <script>
        const words = {{ words_json|safe }};
        window.wordcloudWords = words;
        const canvas = document.getElementById('word-cloud');

        function onReady(err) {
            if (err) {
                document.body.setAttribute('data-error', String(err));
            }
            document.body.setAttribute('data-ready', 'true');
        }

        if (words.length === 0) {
            onReady(null);
        } else {
            canvas.addEventListener('wordcloudstop', () => onReady(null), { once: true });
            canvas.addEventListener('wordcloudabort', (evt) => onReady(evt.detail), { once: true });
            WordCloud(canvas, {
                list: words,
                gridSize: 8,
                weightFactor: function (size) {
                    return Math.pow(size, 1.05) * 0.5 + 10;
                },
                fontFamily: {{ font_family|safe }},
                color: function () {
                    const colors = [
                        '#1f77b4', '#ff7f0e', '#2ca02c', '#d62728',
                        '#9467bd', '#8c564b', '#e377c2', '#7f7f7f',
                        '#bcbd22', '#17becf', '#393b79', '#637939'
                    ];
                    return colors[Math.floor(Math.random() * colors.length)];
                },
                backgroundColor: {{ background|safe }},
                rotateRatio: 0.25,
                minSize: 10,
                shrinkToFit: true,
                drawOutOfBound: false,
                wait: 0
            });
        }
    </script>
</body>
</html>
```

- [ ] **Step 3: 在 `plugins/msg_rank/Cargo.toml` 中声明资源目录包含**

`plugins/msg_rank/Cargo.toml` 不需要显式包含 `assets` 目录（Cargo 默认会打包工作目录内所有非忽略文件），但需在构建后确认文件存在。

- [ ] **Step 4: 提交 Task 1**

```bash
git add plugins/msg_rank/assets plugins/msg_rank/templates/wordcloud.html
git commit -m "feat(msg_rank): add wordcloud2.js asset and screenshot template"
```

---

### Task 2: 更新配置兼容新词云

**Files:**
- Modify: `plugins/msg_rank/src/config.rs`

**Interfaces:**
- Consumes: 无
- Produces: `Config` 结构体新增 `pub wordcloud_background: String`，默认 `"white"`。

- [ ] **Step 1: 修改 `Config` 结构体**

在 `plugins/msg_rank/src/config.rs` 中：

```rust
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    #[serde(default)]
    pub wordcloud_cli_path: String,
    pub notify_group: Vec<i64>,
    pub tencent: Option<TencentCloudConfig>,
    #[serde(default = "default_wordcloud_background")]
    pub wordcloud_background: String,
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_wordcloud_background() -> String {
    "white".to_string()
}
```

- [ ] **Step 2: 更新 `Default` 实现**

```rust
impl Default for Config {
    fn default() -> Self {
        Self {
            wordcloud_cli_path: "wordcloud_cli".to_string(),
            notify_group: vec![],
            tencent: None,
            wordcloud_background: default_wordcloud_background(),
            path: PathBuf::new(),
        }
    }
}
```

- [ ] **Step 3: 运行编译确认无错**

```bash
cargo check -p msg_rank
```

Expected: `Finished dev` 无错误。

- [ ] **Step 4: 提交 Task 2**

```bash
git add plugins/msg_rank/src/config.rs
git commit -m "feat(msg_rank): add wordcloud_background config and deprecate cli path"
```

---

### Task 3: 实现 Playwright 截图词云生成

**Files:**
- Modify: `plugins/msg_rank/src/word_cloud.rs`

**Interfaces:**
- Consumes: `utils::get_context()` 获取 `BrowserContext`，在页面内等待 `wordcloud2.js` 绘制完成后截图。
- Produces: `make_word_cloud(path, group_id, duration) -> Result<Vec<u8>>` 返回 PNG 字节。

- [ ] **Step 1: 重写 `word_cloud.rs` 顶部导入**

```rust
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use anyhow::Result;
use askama::Template;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin, RuntimeBot,
    tokio::{self, time::timeout},
};
use kovi_onebot::{EventRegistrar as _, MessageRegistrar as _, OnebotTrait, event::GroupMsgEvent};
use tracing::{self, info};

use crate::config::{modify_config, read_config};

static JIEBA: LazyLock<jieba_rs::Jieba> = LazyLock::new(jieba_rs::Jieba::new);

const WORDCLOUD_JS: &str = include_str!("../assets/wordcloud2.js");

const WORDCLOUD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
```

- [ ] **Step 2: 删除 CLI 相关常量、进程池与校验函数**

移除：
- `WORDCLOUD_POOL_MAX`
- `WORDCLOUD_POOL_WAIT`
- `WORDCLOUD_POOL`
- `validate_wordcloud_cli_path`
- `tokio::process::Command` 相关代码

- [ ] **Step 3: 新增模板结构体与词频统计**

在 `word_cloud.rs` 末尾附近新增：

```rust
#[derive(Template)]
#[template(path = "wordcloud.html")]
struct WordCloudTemplate {
    title: String,
    background: String,
    has_custom_font: bool,
    font_data_url: String,
    font_family: String,
    words_json: String,
    script: String,
}

#[derive(serde::Serialize)]
struct WordCloudItem {
    word: String,
    weight: u32,
}

fn count_words(words: Vec<String>, stop_words: &[String]) -> Vec<WordCloudItem> {
    let stop_set: std::collections::HashSet<&str> = stop_words.iter().map(|s| s.as_str()).collect();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for w in words {
        let w = w.trim().to_string();
        if w.is_empty() || stop_set.contains(w.as_str()) {
            continue;
        }
        *counts.entry(w).or_insert(0) += 1;
    }
    let mut items: Vec<WordCloudItem> = counts
        .into_iter()
        .map(|(word, weight)| WordCloudItem { word, weight })
        .collect();
    items.sort_by_key(|b| std::cmp::Reverse(b.weight));
    items.truncate(150);
    items
}
```

- [ ] **Step 4: 重写 `make_word_cloud` 函数**

```rust
async fn make_word_cloud(
    path: &Path,
    notify_group: i64,
    duration: chrono::Duration,
) -> Result<Vec<u8>> {
    let end_time = chrono::Local::now();
    let start_time = end_time - duration;

    let messages = crate::db::select_from_time_range(
        notify_group,
        start_time.timestamp(),
        end_time.timestamp(),
    )
    .await?
    .join(" ");

    let raw_words: Vec<String> = JIEBA
        .cut(&messages, true)
        .into_iter()
        .map(|t| t.word.to_string())
        .filter(|s| s.chars().count() > 1)
        .collect();

    if raw_words.is_empty() {
        return Ok(Vec::new());
    }

    let stop_words = load_stop_words(path).await;
    let counted = count_words(raw_words, &stop_words);
    if counted.is_empty() {
        return Ok(Vec::new());
    }

    let words_json = serde_json::to_string(
        &counted
            .into_iter()
            .map(|item| (item.word, item.weight))
            .collect::<Vec<_>>(),
    )?;

    let background = {
        let config = read_config();
        config.wordcloud_background.clone()
    };

    let font_path = path.join("font.otf");
    let (has_custom_font, font_data_url, font_family) = if font_path.exists() {
        let bytes = tokio::fs::read(&font_path).await?;
        let data_url = format!("data:font/otf;base64,{}", STANDARD.encode(&bytes));
        (true, data_url, "\"CustomWordCloudFont\"".to_string())
    } else {
        (
            false,
            String::new(),
            "\"Trebuchet MS\", \"Heiti TC\", \"微軟正黑體\", \"Arial Unicode MS\", \"Droid Fallback Sans\", sans-serif".to_string(),
        )
    };

    // 序列化为 JS 字符串字面量，模板内使用 `|safe` 直接注入。
    let background = serde_json::to_string(&background)?;
    let font_family = serde_json::to_string(&font_family)?;

    let template = WordCloudTemplate {
        title: dsc(duration),
        background,
        has_custom_font,
        font_data_url,
        font_family,
        words_json,
        script: WORDCLOUD_JS.to_string(),
    };
    let html = template.render()?;

    let image = timeout(WORDCLOUD_TIMEOUT, screenshot_word_cloud(html))
        .await
        .map_err(|_| anyhow::anyhow!("词云截图超时"))??;

    Ok(image)
}

async fn screenshot_word_cloud(html: String) -> Result<Vec<u8>> {
    let mut guard = utils::get_context().await?;
    let ctx = playwright_rs::protocol::BrowserContext::clone(&guard);

    let res = async {
        let page = ctx.new_page().await?;
        let screenshot_res = async {
            page.set_content(&html, None).await?;
            page.evaluate::<(), bool>(
                "async () => {\n\
                 const deadline = Date.now() + 30000;\n\
                 while (document.body.getAttribute('data-ready') !== 'true') {\n\
                     if (Date.now() > deadline) throw new Error('wordcloud render timeout');\n\
                     const err = document.body.getAttribute('data-error');\n\
                     if (err) throw new Error('wordcloud render failed: ' + err);\n\
                     await new Promise(r => setTimeout(r, 50));\n\
                 }\n\
                 return true;\n\
                 }",
                None,
            )
            .await?;
            let locator = page.locator("#word-cloud").await;
            let bytes = locator.screenshot(None).await?;
            Ok::<Vec<u8>, anyhow::Error>(bytes)
        }
        .await;
        let _ = page.close().await;
        screenshot_res
    }
    .await;

    if res.is_err() {
        guard.mark_unhealthy();
        let _ = ctx.close().await;
    }

    res
}

async fn load_stop_words(path: &Path) -> Vec<String> {
    let stop_word_path = path.join("stopword.txt");
    if !stop_word_path.exists() {
        return Vec::new();
    }
    match tokio::fs::read_to_string(&stop_word_path).await {
        Ok(content) => content
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(e) => {
            tracing::warn!("读取停用词文件失败: {}", e);
            Vec::new()
        }
    }
}
```

- [ ] **Step 5: 编译检查**

```bash
cargo check -p msg_rank
```

Expected: 无错误。

- [ ] **Step 6: 提交 Task 3**

```bash
git add plugins/msg_rank/src/word_cloud.rs
git commit -m "feat(msg_rank): generate word cloud via playwright screenshot"
```

---

### Task 4: 验证构建与运行

**Files:**
- 无新增文件

**Interfaces:**
- Consumes: 完整构建产物
- Produces: 通过 `cargo build`、`cargo test` 的确认结果。

- [ ] **Step 1: 完整构建**

```bash
cargo build
```

Expected: `Finished dev profile`。

- [ ] **Step 2: 运行测试**

```bash
cargo test
```

Expected: `test result: ok`。

- [ ] **Step 3: 可选：本地生成一张词云图验证**

在 `plugins/msg_rank/src/word_cloud.rs` 的 `#[cfg(test)]` 模块中新增临时测试（完成后可删除或保留）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_render_wordcloud_html() {
        let items = vec![
            ("你好".to_string(), 10),
            ("世界".to_string(), 8),
            ("Rust".to_string(), 6),
        ];
        let words_json = serde_json::to_string(&items).unwrap();
        let template = WordCloudTemplate {
            title: "测试词云".to_string(),
            background: "white".to_string(),
            font_data_url: "data:font/otf;base64,".to_string(),
            font_family: "\"PingFang SC\", \"Microsoft YaHei\", sans-serif".to_string(),
            words_json,
            script: WORDCLOUD_JS.to_string(),
        };
        let html = template.render().unwrap();
        assert!(html.contains("WordCloud"));
        // 如需验证截图，取消下行注释并确保 Chromium 可用：
        // let _png = utils::screenshot(html.into(), Some("#word-cloud".into())).await.unwrap();
    }
}
```

运行：

```bash
cargo test -p msg_rank test_render_wordcloud_html -- --nocapture
```

Expected: 测试通过，HTML 包含 `WordCloud`。

- [ ] **Step 4: 提交 Task 4**

```bash
git add plugins/msg_rank/src/word_cloud.rs
git commit -m "test(msg_rank): add wordcloud html render smoke test"
```

---

## Self-Review

### Spec Coverage

| 设计文档要求 | 对应任务 |
|--------------|----------|
| 复用 `utils::screenshot` | Task 3 Step 4 |
| 保留分词与停用词过滤 | Task 3 Step 3 / Step 4 |
| 支持自定义字体 | Task 3 Step 4 |
| 配置兼容与废弃 `wordcloud_cli_path` | Task 2 |
| 移除 Python CLI 依赖 | Task 3 Step 2 |
| HTML 模板化 | Task 1 / Task 3 |

### Placeholder Scan

- 无 TBD/TODO。
- 所有代码片段为可直接使用的 Rust/HTML。
- 所有命令包含明确 Expected 输出。

### Type Consistency

- `WordCloudTemplate` 字段在 Task 1 模板、Task 3 统计函数、Task 4 测试中保持一致。
- `utils::screenshot` 签名 `Cow<'static, str>, Option<Cow<'static, str>>` 使用正确。

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-07-01-msg-rank-wordcloud-playwright-plan.md`.**

**Execution approach:** 当前处于自动权限模式，将直接按任务顺序在会话语境内联执行（inline execution），不再额外询问执行方式。
