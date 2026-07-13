use std::{
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, Pixel as _, Rgba, imageops::FilterType};
use kovi::{Message, PluginBuilder as plugin, RuntimeBot, tokio};
use kovi_onebot::{EventRegistrar as _, MessageRegistrar as _, OnebotTrait, event::GroupMsgEvent};
use tracing::{self, info};
use wordcloud::{Mask, WordCloud};

use crate::config::{modify_config, read_config};

static JIEBA: LazyLock<jieba_rs::Jieba> = LazyLock::new(jieba_rs::Jieba::new);

/// 输出词云布局尺寸。实际 PNG 通过 2 倍 scale 渲染成 800×800。
const WORDCLOUD_WIDTH: u32 = 400;
const WORDCLOUD_HEIGHT: u32 = 400;
const WORDCLOUD_SCALE: u32 = 2;

/// 最大词数，与 Python wordcloud 默认值对齐。
const MAX_WORDS: usize = 200;
const MAX_WORDCLOUD_INPUT_BYTES: usize = 2 * 1024 * 1024;
static WORDCLOUD_POOL: LazyLock<utils::BoundedPool> = LazyLock::new(|| utils::BoundedPool::new(2));

/// Python wordcloud 默认 viridis 配色。
const WORDCLOUD_COLORS: [Rgba<u8>; 10] = [
    Rgba([68, 1, 84, 255]),
    Rgba([71, 40, 120, 255]),
    Rgba([62, 74, 137, 255]),
    Rgba([49, 104, 142, 255]),
    Rgba([38, 130, 142, 255]),
    Rgba([33, 145, 140, 255]),
    Rgba([53, 183, 121, 255]),
    Rgba([144, 215, 67, 255]),
    Rgba([253, 231, 37, 255]),
    Rgba([92, 38, 134, 255]),
];

pub(crate) async fn init() -> Result<()> {
    help_msg::register_help(
        "wordcloud",
        "启用、禁用或临时生成词云（管理员专用命令）",
        "/wordcloud enable - 启用词云功能\n/wordcloud disable - 禁用词云功能\n/wordcloud once - 立即生成一次今日词云",
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

async fn cmd_handler(event: Arc<GroupMsgEvent>, path: Arc<PathBuf>, bot: Arc<RuntimeBot>) {
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

    let cmd = msg.trim();

    if cmd == "once" {
        event.reply("⏳ 正在生成词云...");
        kovi::spawn(async move {
            send_word_cloud(&bot, group_id, &path, chrono::Duration::days(1), "临时词云").await;
        });
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
            if let Some(admin_id) = bot
                .get_main_admin()
                .ok()
                .and_then(|admin| admin.try_as_i64())
            {
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
    let _permit = WORDCLOUD_POOL
        .acquire(std::time::Duration::from_secs(10))
        .await?;
    let end_time = chrono::Local::now();
    let start_time = end_time - duration;

    let messages = crate::db::select_text_from_time_range(
        notify_group,
        start_time.timestamp(),
        end_time.timestamp(),
        MAX_WORDCLOUD_INPUT_BYTES,
    )
    .await?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    let stop_words = load_stop_words(path).await;
    let background = {
        let config = read_config();
        config.wordcloud_background.clone()
    };
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let raw_words: Vec<String> = JIEBA
            .cut_all(&messages)
            .into_iter()
            .map(|t| t.word.to_string())
            .filter(|s| s.chars().count() > 1)
            .collect();
        let items = count_words(raw_words, &stop_words);
        if items.is_empty() {
            return Ok(Vec::new());
        }
        generate_word_cloud_image(&path, items, &background)
    })
    .await
    .map_err(|e| anyhow::anyhow!("词云后台任务失败: {e}"))?
}

/// 使用 wordcloud-rs 直接生成 PNG 词云图。
fn generate_word_cloud_image(
    path: &Path,
    items: Vec<WordCloudItem>,
    background: &str,
) -> Result<Vec<u8>> {
    let frequencies = items.into_iter().map(|item| (item.word, item.weight));

    let mut builder = WordCloud::builder()
        .dimensions(WORDCLOUD_WIDTH, WORDCLOUD_HEIGHT)
        .scale(WORDCLOUD_SCALE)
        .max_words(MAX_WORDS)
        // 词间距调小，让布局更紧凑。
        .margin(1)
        // Python wordcloud 默认 prefer_horizontal=0.9。
        .prefer_horizontal(0.9)
        // Python wordcloud 默认 min_font_size=4，让高频词与长尾词差距更明显。
        .min_font_size(4.0)
        .max_font_size(120.0)
        .palette(WORDCLOUD_COLORS)
        .background_color(parse_background_color(background));

    // 中文渲染需要 data 目录下的 font.otf 覆盖相应字形。
    let font_path = path.join("font.otf");
    if font_path.exists() {
        builder = builder.font_path(font_path);
    }

    // 加载自定义遮罩（如果 data 目录下存在 mask.png / mask.jpg）。
    if let Some(mask) = load_mask(path)? {
        builder = builder.mask(mask);
    }

    let wordcloud = builder
        .build()
        .map_err(|e| anyhow::anyhow!("初始化词云生成器失败: {e}"))?;
    let image = wordcloud
        .generate_from_frequencies(frequencies)
        .map_err(|e| anyhow::anyhow!("生成词云失败: {e}"))?;

    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("导出 PNG 失败: {e}"))?;
    Ok(png.into_inner())
}

/// 尝试读取 data/mask.png 或 data/mask.jpg 作为词云遮罩。
///
/// 纯白和透明区域禁止放置文字，其他区域允许放置，与 Python wordcloud 的约定一致。
fn load_mask(path: &Path) -> Result<Option<Mask>> {
    for name in ["mask.png", "mask.jpg", "mask.jpeg"] {
        let mask_path = path.join(name);
        if mask_path.exists() {
            let bytes = std::fs::read(&mask_path)
                .map_err(|e| anyhow::anyhow!("读取遮罩失败 {}: {e}", mask_path.display()))?;
            let image = image::load_from_memory(&bytes)
                .map_err(|e| anyhow::anyhow!("解析遮罩失败 {}: {e}", mask_path.display()))?
                .resize_exact(WORDCLOUD_WIDTH, WORDCLOUD_HEIGHT, FilterType::Nearest)
                .to_rgba8();
            let grayscale = GrayImage::from_fn(WORDCLOUD_WIDTH, WORDCLOUD_HEIGHT, |x, y| {
                let pixel = image.get_pixel(x, y);
                if pixel[3] < 128 {
                    Luma([255])
                } else {
                    pixel.to_luma()
                }
            });
            return Ok(Some(Mask::from_luma8(grayscale)));
        }
    }
    Ok(None)
}

/// 将常见颜色名或十六进制字符串统一为 #RRGGBB。
fn normalize_background_color(color: &str) -> String {
    let color = color.trim();
    if color.starts_with('#')
        && color.len() == 7
        && color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return color.to_string();
    }

    match color.to_lowercase().as_str() {
        "white" => "#FFFFFF".to_string(),
        "black" => "#000000".to_string(),
        "red" => "#FF0000".to_string(),
        "green" => "#008000".to_string(),
        "blue" => "#0000FF".to_string(),
        "yellow" => "#FFFF00".to_string(),
        "transparent" => "#FFFFFF".to_string(),
        _ => {
            tracing::warn!("无法解析背景色 '{}', 使用默认白色", color);
            "#FFFFFF".to_string()
        }
    }
}

fn parse_background_color(color: &str) -> Rgba<u8> {
    let color = normalize_background_color(color);
    let parse = |range| u8::from_str_radix(&color[range], 16).unwrap_or(255);
    Rgba([parse(1..3), parse(3..5), parse(5..7), 255])
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
    items.truncate(MAX_WORDS);
    items
}

struct WordCloudItem {
    word: String,
    weight: u32,
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

    #[test]
    fn test_normalize_background_color() {
        assert_eq!(normalize_background_color("#161628"), "#161628");
        assert_eq!(normalize_background_color("white"), "#FFFFFF");
        assert_eq!(normalize_background_color("black"), "#000000");
        assert_eq!(normalize_background_color("unknown"), "#FFFFFF");
        assert_eq!(normalize_background_color("#zzzzzz"), "#FFFFFF");
        assert_eq!(parse_background_color("#161628"), Rgba([22, 22, 40, 255]));
    }

    #[test]
    fn test_wordcloud_generate_direct() {
        let items = vec![
            WordCloudItem {
                word: "rust".to_string(),
                weight: 10,
            },
            WordCloudItem {
                word: "wordcloud".to_string(),
                weight: 8,
            },
            WordCloudItem {
                word: "layout".to_string(),
                weight: 6,
            },
        ];

        let png = generate_word_cloud_image(Path::new("/nonexistent"), items, "white").unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (800, 800));
        assert!(
            image
                .pixels()
                .any(|pixel| *pixel != Rgba([255, 255, 255, 255]))
        );
    }
}
