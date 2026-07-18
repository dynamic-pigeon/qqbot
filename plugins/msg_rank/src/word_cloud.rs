use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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

static RESOURCE_CACHE: LazyLock<ResourceCache> = LazyLock::new(ResourceCache::new);

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

/// 重资源的按需缓存：首次生成词云时加载，空闲超时后自动卸载。
/// 进行中的生成任务持有 `Arc` 克隆，卸载只移除缓存引用，内存随任务结束释放。
struct ResourceCache {
    resources: Mutex<Option<Arc<WordCloudResources>>>,
    /// 每次取用资源递增；空闲回收任务通过比对代数判断期间是否有新取用。
    generation: AtomicU64,
    idle_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ResourceCache {
    fn new() -> Self {
        Self {
            resources: Mutex::new(None),
            generation: AtomicU64::new(0),
            idle_task: Mutex::new(None),
        }
    }

    fn get(&self, font_path: &Path) -> Result<Arc<WordCloudResources>> {
        let snapshot = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let resources = {
            let mut guard = self.resources.lock().unwrap_or_else(|p| p.into_inner());
            match &*guard {
                Some(resources) => Arc::clone(resources),
                None => {
                    info!("加载 jieba 词典与词云字体");
                    let font_bytes = match std::fs::read(font_path) {
                        Ok(bytes) => Some(Arc::from(bytes)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "读取字体失败 {}: {e}",
                                font_path.display()
                            ));
                        }
                    };
                    let resources = Arc::new(WordCloudResources {
                        jieba: Arc::new(jieba_rs::Jieba::new()),
                        font_bytes,
                    });
                    *guard = Some(Arc::clone(&resources));
                    resources
                }
            }
        };
        self.schedule_idle_cleanup(snapshot);
        Ok(resources)
    }

    fn schedule_idle_cleanup(&self, snapshot: u64) {
        // 非 tokio 上下文（如单元测试）无法安排回收任务，此时词典随进程常驻。
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut task = self.idle_task.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(previous) = task.take() {
            previous.abort();
        }
        *task = Some(handle.spawn(try_evict_idle_resources(snapshot)));
    }

    async fn try_evict_idle(&self, snapshot: u64) {
        tokio::time::sleep(RESOURCE_IDLE_TIMEOUT).await;
        self.evict_if_unchanged(snapshot);
    }

    fn evict_if_unchanged(&self, snapshot: u64) {
        // 期间有新的取用 → 不卸载
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }
        let mut guard = self.resources.lock().unwrap_or_else(|p| p.into_inner());
        // 获取锁后再次检查，避免与新取用竞态
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }
        if guard.take().is_some() {
            info!("词云资源空闲超过 {:?}，已卸载", RESOURCE_IDLE_TIMEOUT);
        }
    }
}

/// 后台空闲回收任务入口：从静态缓存读取当前代数并尝试卸载。
async fn try_evict_idle_resources(snapshot: u64) {
    RESOURCE_CACHE.try_evict_idle(snapshot).await;
}

/// 输出词云布局尺寸。实际 PNG 通过 2 倍 scale 渲染成 800×800。
const WORDCLOUD_WIDTH: u32 = 400;
const WORDCLOUD_HEIGHT: u32 = 400;
const WORDCLOUD_SCALE: u32 = 2;

/// 最大词数，与 Python wordcloud 默认值对齐。
const MAX_WORDS: usize = 200;
const MAX_WORDCLOUD_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// 词云生成全局串行：单次生成要加载词典和字体并布局整张画布，
/// 并发生成会让内存峰值成倍叠加。
static WORDCLOUD_POOL: LazyLock<utils::BoundedPool> = LazyLock::new(|| utils::BoundedPool::new(1));

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
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        generate_word_cloud_image(&path, &messages, &stop_words, &background)
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
    path: &Path,
    text: &str,
    stop_words: &[String],
    background: &str,
) -> Result<Vec<u8>> {
    // 生成期间持有资源 Arc：即使空闲回收触发，内存也随本次生成结束才释放。
    let resources = RESOURCE_CACHE.get(&path.join("font.otf"))?;
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

    // 加载自定义遮罩（如果 data 目录下存在 mask.png / mask.jpg）。
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

/// 对 jieba 全模式分词结果直接计数词频。
///
/// 2MB 输入会产生近百万 token，键直接借用输入文本（`Cow::Borrowed`），
/// 只有含大写字母的词才分配小写副本；如果先物化成 `Vec<String>` 再统计，
/// 实测进程 RSS 峰值会超过 600MB。
fn count_frequencies<'a>(
    jieba: &jieba_rs::Jieba,
    text: &'a str,
    stop_words: &[String],
) -> HashMap<Cow<'a, str>, u64> {
    let stop_words: HashSet<String> = stop_words.iter().map(|word| word.to_lowercase()).collect();
    let mut counts: HashMap<Cow<'a, str>, u64> = HashMap::new();
    for token in jieba.cut_all(text) {
        let word = token.word.trim();
        let word_length = word.chars().count();
        // 超长词（如整段链接）只跳过自身，不让单个词导致整次生成失败。
        if !(MIN_WORD_LENGTH..=MAX_WORD_LENGTH).contains(&word_length) {
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

    #[tokio::test]
    async fn resource_cache_evicts_only_when_snapshot_matches() {
        let cache = ResourceCache::new();
        let resources = cache.get(Path::new("/nonexistent/font.otf")).unwrap();
        assert!(cache.resources.lock().unwrap().is_some());

        // 代数不一致（期间有新取用）→ 保留缓存
        let stale = cache.generation.load(Ordering::Relaxed).wrapping_add(1);
        cache.evict_if_unchanged(stale);
        assert!(cache.resources.lock().unwrap().is_some());

        // 代数一致 → 卸载缓存引用；已持有 Arc 的任务不受影响
        let current = cache.generation.load(Ordering::Relaxed);
        cache.evict_if_unchanged(current);
        assert!(cache.resources.lock().unwrap().is_none());
        assert!(!resources.jieba.cut_all("测试").is_empty());
    }

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
        let png = generate_word_cloud_image(Path::new("/nonexistent"), text, &[], "white").unwrap();
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
        let result = generate_word_cloud_image(
            Path::new("/nonexistent"),
            "the the the",
            &stop_words,
            "white",
        )
        .unwrap();
        assert!(result.is_empty());
    }
}
