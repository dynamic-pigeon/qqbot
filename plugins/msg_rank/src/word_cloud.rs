use std::{
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

static JIEBA_CACHE: LazyLock<JiebaCache> = LazyLock::new(JiebaCache::new);

/// jieba 词典加载后约占 56MB 常驻内存，而词云每天只生成几次；
/// 空闲超过此时间后卸载词典，下次生成时重新加载（加载耗时在亚秒级）。
const JIEBA_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// jieba 词典的按需缓存：首次生成词云时加载，空闲超时后自动卸载。
/// 进行中的生成任务持有 `Arc` 克隆，卸载只移除缓存引用，内存随任务结束释放。
struct JiebaCache {
    jieba: Mutex<Option<Arc<jieba_rs::Jieba>>>,
    /// 每次取用词典递增；空闲回收任务通过比对代数判断期间是否有新取用。
    generation: AtomicU64,
    idle_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl JiebaCache {
    fn new() -> Self {
        Self {
            jieba: Mutex::new(None),
            generation: AtomicU64::new(0),
            idle_task: Mutex::new(None),
        }
    }

    fn get(&self) -> Arc<jieba_rs::Jieba> {
        let snapshot = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let jieba = {
            let mut guard = self.jieba.lock().unwrap_or_else(|p| p.into_inner());
            match &*guard {
                Some(jieba) => Arc::clone(jieba),
                None => {
                    info!("加载 jieba 词典");
                    let jieba = Arc::new(jieba_rs::Jieba::new());
                    *guard = Some(Arc::clone(&jieba));
                    jieba
                }
            }
        };
        self.schedule_idle_cleanup(snapshot);
        jieba
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
        *task = Some(handle.spawn(try_evict_idle_jieba(snapshot)));
    }

    async fn try_evict_idle(&self, snapshot: u64) {
        tokio::time::sleep(JIEBA_IDLE_TIMEOUT).await;
        self.evict_if_unchanged(snapshot);
    }

    fn evict_if_unchanged(&self, snapshot: u64) {
        // 期间有新的取用 → 不卸载
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }
        let mut guard = self.jieba.lock().unwrap_or_else(|p| p.into_inner());
        // 获取锁后再次检查，避免与新取用竞态
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }
        if guard.take().is_some() {
            info!("jieba 词典空闲超过 {:?}，已卸载", JIEBA_IDLE_TIMEOUT);
        }
    }
}

/// 后台空闲回收任务入口：从静态缓存读取当前代数并尝试卸载。
async fn try_evict_idle_jieba(snapshot: u64) {
    JIEBA_CACHE.try_evict_idle(snapshot).await;
}

/// 输出词云布局尺寸。实际 PNG 通过 2 倍 scale 渲染成 800×800。
const WORDCLOUD_WIDTH: u32 = 400;
const WORDCLOUD_HEIGHT: u32 = 400;
const WORDCLOUD_SCALE: u32 = 2;

/// 最大词数，与 Python wordcloud 默认值对齐。
const MAX_WORDS: usize = 200;
const MAX_WORDCLOUD_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// 词云生成全局串行：单次生成的瞬时内存高（16MB 字体 + 全模式分词的大量分配），
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

/// 使用 wordcloud-rs 直接处理原始文本生成 PNG 词云图。
fn generate_word_cloud_image(
    path: &Path,
    text: &str,
    stop_words: &[String],
    background: &str,
) -> Result<Vec<u8>> {
    // 生成期间持有词典 Arc：即使空闲回收触发，内存也随本次生成结束才释放。
    let jieba = JIEBA_CACHE.get();
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
        .background_color(parse_background_color(background))
        // 中文分词：jieba 全模式闭包替换默认的英文单词边界。
        .tokenizer(move |text: &str| {
            jieba
                .cut_all(text)
                .into_iter()
                .map(|t| t.word.to_string())
                .collect()
        })
        .stopwords(stop_words.iter().map(String::as_str))
        // 多字符词阈值，与 Python wordcloud 习惯一致。
        .min_word_length(2);

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
    // 消息全是停用词、表情或单字时没有有效词可渲染，返回空结果跳过本次发送。
    let image = match wordcloud.generate(text) {
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
    async fn jieba_cache_evicts_only_when_snapshot_matches() {
        let cache = JiebaCache::new();
        let jieba = cache.get();
        assert!(cache.jieba.lock().unwrap().is_some());

        // 代数不一致（期间有新取用）→ 保留缓存
        let stale = cache.generation.load(Ordering::Relaxed).wrapping_add(1);
        cache.evict_if_unchanged(stale);
        assert!(cache.jieba.lock().unwrap().is_some());

        // 代数一致 → 卸载缓存引用；已持有 Arc 的任务不受影响
        let current = cache.generation.load(Ordering::Relaxed);
        cache.evict_if_unchanged(current);
        assert!(cache.jieba.lock().unwrap().is_none());
        assert!(!jieba.cut_all("测试").is_empty());
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
