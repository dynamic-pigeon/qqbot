use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, Pixel as _, Rgba, imageops::FilterType};
use kovi::{Message, PluginBuilder as plugin, RuntimeBot, tokio};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use tracing::{self, info};
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};
use wordcloud::{Mask, WordCloud, WordCloudError};

use crate::config::{modify_config, read_config};

static RESOURCE_MANAGER: tokio::sync::OnceCell<utils::ResourceManager<WordCloudResources>> =
    tokio::sync::OnceCell::const_new();

/// 生成词云的重资源（jieba 词典 ~54MB + 字体 ~16MB）加载后常驻内存，
/// 而词云每天只生成几次；空闲超过此时间后卸载，下次生成时重新加载（加载耗时在亚秒级）。
const RESOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次词云生成所需的重资源。
struct WordCloudResources {
    jieba: Arc<jieba_rs::Jieba>,
    /// data/font.otf 的字节；文件不存在时为 None，回退到 wordcloud 内置字体。
    /// fontdue 每次生成仍会解析字体，但免去了每次从磁盘读 16MB。
    font_bytes: Option<Arc<[u8]>>,
}

async fn resource_manager(
    font_path: PathBuf,
) -> &'static utils::ResourceManager<WordCloudResources> {
    RESOURCE_MANAGER
        .get_or_init(move || async move {
            utils::ResourceManager::new_with_destructor(
                RESOURCE_IDLE_TIMEOUT,
                move || {
                    let font_path = font_path.clone();
                    async move {
                        tokio::task::spawn_blocking(move || load_word_cloud_resources(&font_path))
                            .await
                            .map_err(|e| anyhow::anyhow!("词云资源加载任务失败: {e}"))?
                    }
                },
                |resources| async move {
                    info!("词云资源空闲超过 {:?}，已卸载", RESOURCE_IDLE_TIMEOUT);
                    drop(resources);
                },
            )
        })
        .await
}

fn load_word_cloud_resources(font_path: &Path) -> Result<WordCloudResources> {
    info!("加载 jieba 词典与词云字体");
    let font_bytes = match std::fs::read(font_path) {
        Ok(bytes) => Some(Arc::from(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::anyhow!("读取字体失败 {}: {e}", font_path.display()));
        }
    };
    Ok(WordCloudResources {
        jieba: Arc::new(jieba_rs::Jieba::new()),
        font_bytes,
    })
}

/// 输出词云布局尺寸。实际 PNG 通过 2 倍 scale 渲染成 800×800。
const WORDCLOUD_WIDTH: u32 = 400;
const WORDCLOUD_HEIGHT: u32 = 400;
const WORDCLOUD_SCALE: u32 = 2;

/// 最大词数，与 Python wordcloud 默认值对齐。
const MAX_WORDS: usize = 200;
const MAX_WORDCLOUD_INPUT_BYTES: usize = 2 * 1024 * 1024;
static WORDCLOUD_POOL: LazyLock<utils::BoundedPool> =
    LazyLock::new(|| utils::BoundedPool::new(crate::config::static_config().wordcloud_concurrency));

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

pub(crate) async fn init(bot: Arc<RuntimeBot>, path: Arc<PathBuf>) -> Result<()> {
    for schedule in &crate::config::static_config().wordcloud {
        let bot = Arc::clone(&bot);
        let path = Arc::clone(&path);
        let days = schedule.days;
        let title = schedule.title.clone();
        plugin::cron(&schedule.cron, move || {
            let path = &path;
            let bot = &bot;
            let config = read_config();
            for &group_id in &config.notify_group {
                let bot = Arc::clone(bot);
                let path = Arc::clone(path);
                let title = title.clone();
                kovi::spawn(async move {
                    send_word_cloud(&bot, group_id, &path, chrono::Duration::days(days), &title)
                        .await;
                });
            }
            async move {}
        })
        .unwrap();
    }
    Ok(())
}

pub(crate) fn wordcloud_command(path: Arc<PathBuf>) -> Command {
    let once_path = Arc::clone(&path);
    Command::new("/wordcloud")
        .description("启用、禁用或临时生成词云")
        .usage("/wordcloud <once|enable|disable|status>")
        .scope(MessageScope::Group)
        .permission(Permission::BotAdmin)
        .subcommand(
            Command::new("once")
                .description("立即生成一次今日词云")
                .usage("/wordcloud once")
                .handler(move |ctx| {
                    let path = Arc::clone(&once_path);
                    async move { wordcloud_once(ctx, path).await }
                }),
        )
        .subcommand(
            Command::new("enable")
                .description("启用本群词云")
                .usage("/wordcloud enable")
                .handler(wordcloud_enable),
        )
        .subcommand(
            Command::new("disable")
                .description("停用本群词云")
                .usage("/wordcloud disable")
                .handler(wordcloud_disable),
        )
        .subcommand(
            Command::new("status")
                .description("查看本群词云状态")
                .usage("/wordcloud status")
                .handler(wordcloud_status),
        )
}

async fn wordcloud_once(ctx: CommandContext, path: Arc<PathBuf>) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.event().group_id.expect("群命令已通过范围校验");
    let bot = Arc::clone(ctx.bot());
    ctx.reply("⏳ 正在生成词云...");
    kovi::spawn(async move {
        send_word_cloud(&bot, group_id, &path, chrono::Duration::days(1), "临时词云").await;
    });
    Ok(())
}

async fn wordcloud_enable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.event().group_id.expect("群命令已通过范围校验");
    modify_config(|config| {
        if !config.notify_group.contains(&group_id) {
            config.notify_group.push(group_id);
        }
    })
    .await
    .map_err(CommandError::internal)?;
    ctx.reply("启用成功");
    Ok(())
}

async fn wordcloud_disable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.event().group_id.expect("群命令已通过范围校验");
    modify_config(|config| {
        config.notify_group.retain(|&id| id != group_id);
    })
    .await
    .map_err(CommandError::internal)?;
    ctx.reply("停用成功");
    Ok(())
}

async fn wordcloud_status(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.event().group_id.expect("群命令已通过范围校验");
    let enabled = read_config().notify_group.contains(&group_id);
    ctx.reply(if enabled {
        "词云功能已启用"
    } else {
        "词云功能未启用"
    });
    Ok(())
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
    // 生成全局串行，排队的任务需等待前一个个完成，超时时间需覆盖最坏等待。
    let _permit = WORDCLOUD_POOL
        .acquire(std::time::Duration::from_secs(10 * 60))
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
    let resources = resource_manager(path.join("font.otf")).await.get().await?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        generate_word_cloud_image(&resources, &path, &messages, &stop_words, &background)
    })
    .await
    .map_err(|e| anyhow::anyhow!("词云后台任务失败: {e}"))?
}

/// 词频统计的最小词长，多字符词阈值与 Python wordcloud 习惯一致。
const MIN_WORD_LENGTH: usize = 2;
/// 词频统计的最大词长，与 wordcloud 库 max_word_length 的默认值一致。
const MAX_WORD_LENGTH: usize = 256;

/// 使用 wordcloud-rs 生成 PNG 词云图。
fn generate_word_cloud_image(
    resources: &WordCloudResources,
    path: &Path,
    text: &str,
    stop_words: &[String],
    background: &str,
) -> Result<Vec<u8>> {
    let frequencies = count_frequencies(&resources.jieba, text, stop_words);

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
    if let Some(font_bytes) = &resources.font_bytes {
        builder = builder.font_data(Arc::clone(font_bytes));
    }

    if let Some(mask) = load_mask(path)? {
        builder = builder.mask(mask);
    }

    let wordcloud = builder
        .build()
        .map_err(|e| anyhow::anyhow!("初始化词云生成器失败: {e}"))?;
    // 消息全是停用词、表情或单字时没有有效词可渲染，返回空结果跳过本次发送。
    let image = match wordcloud.generate_from_frequencies(frequencies) {
        Ok(image) => image,
        Err(WordCloudError::EmptyInput) => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("生成词云失败: {e}")),
    };

    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("导出 PNG 失败: {e}"))?;
    Ok(png.into_inner())
}

/// 精确模式分词后计数词频。HMM 打开，避免未登录词被拆成单字。
///
/// 键直接借用输入文本（`Cow::Borrowed`），只有含大写字母的词才分配小写副本。
fn count_frequencies<'a>(
    jieba: &jieba_rs::Jieba,
    text: &'a str,
    stop_words: &[String],
) -> HashMap<Cow<'a, str>, u64> {
    let stop_words: HashSet<String> = stop_words.iter().map(|word| word.to_lowercase()).collect();
    let mut counts: HashMap<Cow<'a, str>, u64> = HashMap::new();
    for token in jieba.cut(text, true) {
        let word = token.word.trim();
        // 超长词（如整段链接）只跳过自身，不让单个词导致整次生成失败。
        if !(MIN_WORD_LENGTH..=MAX_WORD_LENGTH).contains(&word.chars().count()) {
            continue;
        }
        let key: Cow<'a, str> = if word.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(word.to_lowercase())
        } else {
            Cow::Borrowed(word)
        };
        if stop_words.contains(key.as_ref())
            || key
                .chars()
                .filter(|character| !character.is_whitespace())
                .all(|character| character.is_numeric())
        {
            continue;
        }
        *counts.entry(key).or_default() += 1;
    }
    counts
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

#[cfg(test)]
mod tests {
    use super::*;
    use utils::command::{Permission, ResolveOutcome, RouteError};

    #[test]
    fn command_tree_registers_admin_group_subcommands() {
        let tree = utils::command::CommandTree::new(vec![wordcloud_command(Arc::new(
            PathBuf::from("/tmp"),
        ))])
        .unwrap();

        for name in ["once", "enable", "disable", "status"] {
            let ResolveOutcome::Matched(command) = tree.resolve(&format!("/wordcloud {name}"))
            else {
                panic!("expected /wordcloud {name} to resolve");
            };
            assert_eq!(command.permission(), Permission::BotAdmin);
            assert_eq!(command.scope(), utils::command::MessageScope::Group);
        }

        assert!(matches!(
            tree.resolve("/wordcloud"),
            ResolveOutcome::Error(RouteError::MissingSubcommand { .. })
        ));
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
        let text = "rust rust rust rust wordcloud wordcloud layout";
        let resources = load_word_cloud_resources(Path::new("/nonexistent/font.otf")).unwrap();
        let png =
            generate_word_cloud_image(&resources, Path::new("/nonexistent"), text, &[], "white")
                .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (800, 800));
        assert!(
            image
                .pixels()
                .any(|pixel| *pixel != Rgba([255, 255, 255, 255]))
        );
    }

    #[test]
    fn test_wordcloud_no_usable_words_returns_empty() {
        let stop_words = vec!["the".to_string()];
        let resources = load_word_cloud_resources(Path::new("/nonexistent/font.otf")).unwrap();
        let result = generate_word_cloud_image(
            &resources,
            Path::new("/nonexistent"),
            "the the the",
            &stop_words,
            "white",
        )
        .unwrap();
        assert!(result.is_empty());
    }
}
