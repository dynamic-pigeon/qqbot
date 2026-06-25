use kovi::tokio;
use kovi_onebot::{OneBotDriver, load_local_conf};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化 tracing 订阅器，支持按库设置日志等级
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 默认配置：不同库设置不同的日志级别
        EnvFilter::new(
            "info,\
            qqbot=info,\
            msg_rank=debug,\
            yu_gi_oh=debug,\
            bilibili=debug,\
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

    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

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

    let driver_config = load_local_conf()?;
    let driver = OneBotDriver::new(driver_config);

    let bot = kovi::build_bot!(driver; kovi_plugin_cmd, msg_rank, help_msg, markdown, yu_gi_oh, bilibili);

    bot.run().await;
    Ok(())
}
