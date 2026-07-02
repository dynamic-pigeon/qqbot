use kovi::tokio;
use kovi_onebot::{OneBotDriver, load_local_conf};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 加载 .env 文件（如存在）；系统环境变量优先级高于 .env。
    // dotenvy 默认在当前目录及向上查找 .env 文件。
    let _ = dotenvy::dotenv();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
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

    let bot =
        kovi::build_bot!(driver; kovi_plugin_cmd, msg_rank, help_msg, markdown, yu_gi_oh, bilibili);

    bot.run().await;
    Ok(())
}
