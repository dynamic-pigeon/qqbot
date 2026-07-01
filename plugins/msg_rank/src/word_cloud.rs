use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::Duration,
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

/// 词云绘制库源码，编译时嵌入，避免运行时依赖网络或外部文件。
const WORDCLOUD_JS: &str = include_str!("../assets/wordcloud2.js");

/// 截图整体超时。wordcloud2.js 绘制加上浏览器渲染通常很快，保留 60s 以应对首次启动浏览器。
const WORDCLOUD_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) async fn init() -> Result<()> {
    help_msg::register_help(
        "wordcloud",
        "启用或禁用词云功能（管理员专用命令）",
        "/wordcloud enable - 启用词云功能\n/wordcloud disable - 禁用词云功能",
    )
    .await;
    let bot = plugin::get_runtime_bot();
    let bot_ = Arc::clone(&bot);
    let path = Arc::new(bot.get_data_path());
    let path_ = Arc::clone(&path);
    plugin::on_group_msg(move |event| {
        let bot = Arc::clone(&bot_);
        let path = Arc::clone(&path_);
        cmd_handler(event, path, bot)
    });
    let bot_ = Arc::clone(&bot);
    let path_ = Arc::clone(&path);
    plugin::cron("0 21 * * *", move || {
        let path = &path_;
        let bot = &bot_;
        let config = read_config();
        let notify_group = &config.notify_group;

        for &group_id in notify_group {
            let bot = Arc::clone(bot);
            let path = Arc::clone(path);
            kovi::spawn(async move {
                send_word_cloud(&bot, group_id, &path, chrono::Duration::days(1), "今日词云").await;
            });
        }
        async move {}
    })
    .unwrap();
    let bot_ = Arc::clone(&bot);
    let path_ = Arc::clone(&path);
    plugin::cron("0 10 * * 6", move || {
        let path = &path_;
        let bot = &bot_;
        let config = read_config();
        let notify_group = &config.notify_group;

        for &group_id in notify_group {
            let bot = Arc::clone(bot);
            let path = Arc::clone(path);
            kovi::spawn(async move {
                send_word_cloud(&bot, group_id, &path, chrono::Duration::days(7), "上周词云").await;
            });
        }
        async move {}
    })
    .unwrap();
    Ok(())
}

async fn cmd_handler(event: Arc<GroupMsgEvent>, _path: Arc<PathBuf>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id;

    let text = event.borrow_text().unwrap_or_default();
    let msg = text.trim();

    let Some(msg) = msg.strip_prefix("/wordcloud ") else {
        return;
    };

    if !bot
        .get_all_admin()
        .unwrap_or_default()
        .iter()
        .any(|id| id.try_as_i64() == Some(event.user_id))
    {
        event.reply("❌ 管理员专用命令，普通用户无法使用");
        return;
    }

    let exe_cmd = async |cmd: &str, group_id: i64| -> Result<&str> {
        match cmd {
            "enable" => {
                modify_config(|config| {
                    if !config.notify_group.contains(&group_id) {
                        config.notify_group.push(group_id);
                    }
                })
                .await?;
                Ok("启用成功")
            }
            "disable" => {
                modify_config(|config| {
                    config.notify_group.retain(|&id| id != group_id);
                })
                .await?;
                Ok("停用成功")
            }
            "status" => {
                let config = read_config();
                if config.notify_group.contains(&group_id) {
                    Ok("词云功能已启用")
                } else {
                    Ok("词云功能未启用")
                }
            }
            _ => {
                anyhow::bail!("未知命令: {}", cmd);
            }
        }
    };

    let cmd = msg.trim();
    match exe_cmd(cmd, group_id).await {
        Ok(res) => {
            event.reply(res);
        }
        Err(e) => {
            tracing::error!("执行命令失败: {}", e);
            event.reply(format!("执行命令失败: {}", e));
        }
    }
}

async fn send_word_cloud(
    bot: &RuntimeBot,
    group_id: i64,
    path: &Path,
    duration: chrono::Duration,
    dsc: &str,
) {
    let image = match make_word_cloud(path, group_id, duration).await {
        Ok(image) if !image.is_empty() => image,
        Ok(_) => {
            info!("word cloud is empty, group_id: {}", group_id);
            return;
        }
        Err(e) => {
            tracing::error!("make word cloud failed: {}, group_id: {}", e, group_id);
            if let Some(admin_id) = bot.get_main_admin().ok().and_then(|admin| admin.try_as_i64()) {
                bot.send_private_msg(
                    admin_id,
                    format!("make word cloud failed: {}, group_id: {}", e, group_id),
                );
            }
            return;
        }
    };

    info!("send word cloud to group: {}", group_id);

    let image_base64 = STANDARD.encode(&image);
    let image = format!("base64://{}", image_base64);
    bot.send_group_msg(group_id, Message::new().add_text(dsc).add_image(&image));
}

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

    let background = {
        let config = read_config();
        config.wordcloud_background.clone()
    };

    let html = render_word_cloud_html(path, dsc(duration), background, counted).await?;

    let image = timeout(WORDCLOUD_TIMEOUT, screenshot_word_cloud(html)).await
        .map_err(|_| anyhow::anyhow!("词云截图超时"))??;

    Ok(image)
}

/// 根据时间跨度生成标题描述。
fn dsc(duration: chrono::Duration) -> String {
    let days = duration.num_days();
    if days >= 7 {
        "上周词云".to_string()
    } else {
        "今日词云".to_string()
    }
}

async fn screenshot_word_cloud(html: String) -> Result<Vec<u8>> {
    let mut guard = utils::get_context().await?;
    // 克隆 BrowserContext handle，避免后续可变借用 guard 冲突。
    let ctx = playwright_rs::protocol::BrowserContext::clone(&guard);

    let res = async {
        let page = ctx.new_page().await?;
        let screenshot_res = async {
            page.set_content(&html, None).await?;
            // 等待 wordcloud2.js 绘制完成（body 上设置 data-ready）。
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

fn count_words(words: Vec<String>, stop_words: &[String]) -> Vec<WordCloudItem> {
    let stop_set: std::collections::HashSet<&str> =
        stop_words.iter().map(|s| s.as_str()).collect();
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
    items.truncate(250);
    items
}

#[derive(serde::Serialize)]
struct WordCloudItem {
    word: String,
    weight: u32,
}

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

async fn render_word_cloud_html(
    path: &Path,
    title: String,
    background: String,
    items: Vec<WordCloudItem>,
) -> Result<String> {
    let font_path = path.join("font.otf");
    let (has_custom_font, font_data_url, font_family) = if font_path.exists() {
        let bytes = tokio::fs::read(&font_path).await?;
        let data_url = format!("data:font/otf;base64,{}", STANDARD.encode(&bytes));
        (true, data_url, "\"CustomWordCloudFont\"".to_string())
    } else {
        (
            false,
            String::new(),
            "\"STHeiti\", \"Heiti TC\", \"Arial Unicode MS\", \"Microsoft YaHei\", \"PingFang SC\", sans-serif".to_string(),
        )
    };

    let words_json = serde_json::to_string(
        &items
            .into_iter()
            .map(|item| (item.word, item.weight))
            .collect::<Vec<_>>(),
    )?;

    // 作为 JS 字符串字面量序列化，避免模板注入和引号问题。
    let background = serde_json::to_string(&background)?;
    let font_family = serde_json::to_string(&font_family)?;

    let template = WordCloudTemplate {
        title,
        background,
        has_custom_font,
        font_data_url,
        font_family,
        words_json,
        script: WORDCLOUD_JS.to_string(),
    };

    Ok(template.render()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_words_orders_by_weight() {
        let words = vec![
            "rust".to_string(),
            "rust".to_string(),
            "go".to_string(),
            "go".to_string(),
            "go".to_string(),
        ];
        let items = count_words(words, &[]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].word, "go");
        assert_eq!(items[0].weight, 3);
        assert_eq!(items[1].word, "rust");
        assert_eq!(items[1].weight, 2);
    }

    #[test]
    fn test_count_words_respects_stop_words() {
        let words = vec!["rust".to_string(), "the".to_string(), "the".to_string()];
        let items = count_words(words, &["the".to_string()]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].word, "rust");
    }

    #[tokio::test]
    async fn test_render_wordcloud_html_contains_script() {
        let items = vec![
            WordCloudItem {
                word: "你好".to_string(),
                weight: 10,
            },
            WordCloudItem {
                word: "世界".to_string(),
                weight: 5,
            },
        ];
        let html = render_word_cloud_html(Path::new("/nonexistent"), "测试".to_string(), "white".to_string(), items)
            .await
            .unwrap();
        assert!(html.contains("WordCloud"));
        assert!(html.contains("你好"));
        assert!(html.contains("data-ready"));
    }

    /// 直接启动 Playwright 验证词云截图。需要 Chromium 环境，默认忽略。
    /// 手动运行：cargo test -p msg_rank test_wordcloud_screenshot_direct -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_wordcloud_screenshot_direct() {
        let items: Vec<WordCloudItem> = (0..50)
            .map(|i| WordCloudItem {
                word: format!("word{}", i),
                weight: (50 - i) as u32,
            })
            .collect();
        let html = render_word_cloud_html(
            Path::new("/nonexistent"),
            "截图测试".to_string(),
            "white".to_string(),
            items,
        )
        .await
        .unwrap();

        let playwright = playwright_rs::Playwright::launch().await.unwrap();
        let browser = playwright
            .chromium()
            .launch_with_options(
                playwright_rs::LaunchOptions::new()
                    .headless(true)
                    .args(vec![
                        "--disable-dev-shm-usage".to_string(),
                        "--disable-background-networking".to_string(),
                        "--disable-default-apps".to_string(),
                        "--disable-extensions".to_string(),
                        "--disable-sync".to_string(),
                        "--disable-translate".to_string(),
                        "--no-first-run".to_string(),
                        "--mute-audio".to_string(),
                        "--password-store=basic".to_string(),
                        "--use-mock-keychain".to_string(),
                    ]),
            )
            .await
            .unwrap();
        let viewport = playwright_rs::protocol::Viewport {
            width: 1920,
            height: 1080,
        };
        let opts = playwright_rs::protocol::BrowserContextOptions::builder()
            .viewport(viewport)
            .build();
        let context = browser.new_context_with_options(opts).await.unwrap();
        context.set_default_timeout(30_000.0).await;
        let page = context.new_page().await.unwrap();
        page.set_content(&html, None).await.unwrap();
        let diag: serde_json::Value = page
            .evaluate::<(), serde_json::Value>(
                "async () => {\n\
                 const info = {\n\
                     hasWordCloud: typeof WordCloud !== 'undefined',\n\
                     isSupported: typeof WordCloud !== 'undefined' && WordCloud.isSupported,\n\
                     wordCount: (function() {\n\
                         try { return window.wordcloudWords ? window.wordcloudWords.length : 'no window.wordcloudWords'; }\n\
                         catch (e) { return String(e); }\n\
                     })(),\n\
                     bodyReady: document.body.getAttribute('data-ready'),\n\
                     bodyError: document.body.getAttribute('data-error')\n\
                 };\n\
                 const deadline = Date.now() + 30000;\n\
                 while (document.body.getAttribute('data-ready') !== 'true') {\n\
                     if (Date.now() > deadline) {\n\
                         info.timeout = true;\n\
                         return info;\n\
                     }\n\
                     if (info.bodyError) {\n\
                         info.renderError = info.bodyError;\n\
                         return info;\n\
                     }\n\
                     await new Promise(r => setTimeout(r, 50));\n\
                 }\n\
                 info.ready = true;\n\
                 return info;\n\
                 }",
                None,
            )
            .await
            .unwrap();
        if diag.get("ready").and_then(|v| v.as_bool()) != Some(true) {
            panic!("wordcloud render did not finish: {:?}", diag);
        }
        let locator = page.locator("#word-cloud").await;
        let png = locator.screenshot(None).await.unwrap();
        page.close().await.unwrap();

        assert!(!png.is_empty());
        tokio::fs::write("/tmp/wordcloud_test.png", &png).await.unwrap();
    }
}
