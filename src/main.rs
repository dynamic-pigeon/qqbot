use kovi::tokio;
use kovi_onebot::{OneBotDriver, load_local_conf};
use tracing_appender::rolling::Rotation;
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

#[cfg(unix)]
fn restrict_sensitive_file(path: impl AsRef<std::path::Path>) {
    use std::os::unix::fs::PermissionsExt as _;

    let path = path.as_ref();
    if !path.exists() {
        return;
    }
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path.display(), "无法收紧敏感文件权限: {error}");
    }
}

#[cfg(not(unix))]
fn restrict_sensitive_file(_path: impl AsRef<std::path::Path>) {}

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

    let file_appender = tracing_appender::rolling::Builder::new()
        .filename_prefix("bot")
        .filename_suffix("log")
        .max_log_files(30)
        .rotation(Rotation::DAILY)
        .build("./logs")
        .unwrap();

    // 日志 channel 按行数预分配 slot：4096 行约 130KB 常驻内存，足够覆盖日常日志突发。
    let (non_blocking, _guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(4096)
        .finish(file_appender);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .with(
            fmt::layer()
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
    let _ = utils::config::value();

    let driver_config = load_local_conf()?;
    let driver = OneBotDriver::new(driver_config);

    // 应用已安装 tracing subscriber，关掉 Kovi 的全局 logger，避免重复注册。
    let kovi_config = kovi::load_local_conf()?;
    let bot = bot::build_bot(kovi_config, driver);

    bot.run().await;
    Ok(())
}
