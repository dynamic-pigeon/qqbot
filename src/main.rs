use kovi::tokio;
use kovi_onebot::{OneBotDriver, load_local_conf};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

// musl 系统分配器对词云、截图这类周期性大分配几乎不归还内存，
// jemalloc / mimalloc 会在分配活动结束后把空闲页还给 OS。
// 两者只能选一个作为全局分配器。
#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("features `jemalloc` and `mimalloc` are mutually exclusive");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 按本地日期滚动的日志文件 writer：tracing-appender 的 DAILY 只认
/// UTC 日界（本地早八点才切文件，文件名与内容错位八小时）。文件名
/// `<prefix>.<本地日期>.log`，切日即换文件；启动时按文件名排序清理，
/// 只保留最近 30 份。
struct DailyLocalWriter {
    dir: std::path::PathBuf,
    prefix: String,
    state: std::sync::Mutex<(String, std::fs::File)>,
}

impl DailyLocalWriter {
    fn new(dir: impl Into<std::path::PathBuf>, prefix: &str) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let today = Self::today();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{prefix}.{today}.log")))?;
        Self::prune(&dir, prefix);
        Ok(Self {
            dir,
            prefix: prefix.to_owned(),
            state: std::sync::Mutex::new((today, file)),
        })
    }

    fn today() -> String {
        let format = time::format_description::parse_owned::<2>("[year]-[month]-[day]")
            .expect("日期格式合法");
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        now.format(&format).expect("日期格式化")
    }

    /// 删除最旧的日志，只留最近 30 份。
    fn prune(dir: &std::path::Path, prefix: &str) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut logs: Vec<_> = entries
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| {
                name.to_string_lossy().starts_with(prefix)
                    && name.to_string_lossy().ends_with(".log")
            })
            .collect();
        let keep = 30;
        if logs.len() > keep {
            logs.sort();
            for name in &logs[..logs.len() - keep] {
                let _ = std::fs::remove_file(dir.join(name));
            }
        }
    }
}

impl std::io::Write for DailyLocalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let today = Self::today();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if today != state.0 {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("{}.{today}.log", self.prefix)))?;
            *state = (today, file);
        }
        state.1.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.1.flush()
    }
}

fn restrict_sensitive_file(path: impl AsRef<std::path::Path>) {
    let path = path.as_ref();
    if !path.exists() {
        return;
    }
    if let Err(error) = utils::restrict_mode_0600(path) {
        tracing::warn!(path = %path.display(), "无法收紧敏感文件权限: {error}");
    }
}

// 事件入口是单条 WebSocket，词云/sqlite 等重活已在 spawn_blocking 或连接线程上；
// current_thread 只占一条异步线程，避免按 CPU 数拉 worker 抬高空闲 RSS。
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 系统环境变量优先于 .env；dotenvy 从当前目录向上查找。
    let _ = dotenvy::dotenv();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,\
            qqbot=info,\
            msg_rank=debug,\
            yu_gi_oh=debug,\
            bilibili=debug,\
            image_lib=debug,\
            utils=debug",
        )
    });

    let rolling = DailyLocalWriter::new("./logs", "bot")?;

    // 日志 channel 按行数预分配 slot：4096 行约 130KB 常驻内存，足够覆盖日常日志突发。
    let (non_blocking, _guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(4096)
        .finish(rolling);

    // 时间戳用本地时区（服务器 CST），读日志不用再做 +8 换算。
    let format = time::format_description::parse_owned::<2>(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:6] [offset_hour sign:mandatory]:[offset_minute]",
    )
    .expect("日志时间格式合法");
    let timer = tracing_subscriber::fmt::time::LocalTime::new(format);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_timer(timer.clone())
                .with_target(true)
                .with_level(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .with(
            fmt::layer()
                .with_timer(timer)
                .with_target(true)
                .with_level(true)
                .with_line_number(true)
                .with_thread_ids(true),
        )
        .init();

    restrict_sensitive_file(".env");
    restrict_sensitive_file("kovi.conf.toml");
    restrict_sensitive_file("config.toml");

    // 配置损坏时在插件启动前失败，避免跑到第一次下图才 panic。
    utils::config::preload();

    let driver_config = load_local_conf()?;
    let driver = OneBotDriver::new(driver_config);

    // 应用已安装 tracing subscriber，关掉 Kovi 的全局 logger，避免重复注册。
    let kovi_config = kovi::load_local_conf()?;
    let bot = bot::build_bot(kovi_config, driver);

    bot.run().await;
    Ok(())
}
