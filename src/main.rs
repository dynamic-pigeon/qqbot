use kovi::build_bot;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() {
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

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_line_number(true)
                .with_thread_ids(true),
        )
        .init();

    build_bot!(
        kovi_plugin_cmd,
        msg_rank,
        help_msg,
        markdown,
        yu_gi_oh,
        bilibili
    )
    .run();
}
