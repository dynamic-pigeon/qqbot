use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use itertools::Itertools as _;
use kovi::{
    Message, PluginBuilder as plugin, RuntimeBot,
    tokio::{self, io::AsyncWriteExt as _, time::timeout},
};
use kovi_onebot::{EventRegistrar as _, MessageRegistrar as _, OnebotTrait, event::GroupMsgEvent};
use tracing::{self, info};

use crate::config::{modify_config, read_config};

static JIEBA: LazyLock<jieba_rs::Jieba> = LazyLock::new(jieba_rs::Jieba::new);

/// wordcloud_cli 进程池容量。每日 cron 同时给所有 notify_group 跑，存在 N 个并发子进程的
/// 风险（每个都跑分词 + 起 chromium/cli + 内存塞几 MB stdin），把并发上限压到 2。
const WORDCLOUD_POOL_MAX: usize = 2;

/// 获取 wordcloud 进程池许可的最长等待。超过此值直接报错返回，让上层 cron 路径走
/// `send_word_cloud` 既有的失败→私聊 admin 通知路径。
const WORDCLOUD_POOL_WAIT: Duration = Duration::from_secs(5);

static WORDCLOUD_POOL: LazyLock<utils::BoundedPool> =
    LazyLock::new(|| utils::BoundedPool::new(WORDCLOUD_POOL_MAX));

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
    plugin::cron("0 10 * * 6", move || {
        let path = &path;
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
    // 进程池 gate：池满则排队等 ≤ 5s，超时直接 bail；许可在函数结尾 drop 自动归还。
    let _permit = WORDCLOUD_POOL.acquire(WORDCLOUD_POOL_WAIT).await?;

    let end_time = chrono::Local::now();
    let start_time = end_time - duration;

    let messages = crate::db::select_from_time_range(
        notify_group,
        start_time.timestamp(),
        end_time.timestamp(),
    )
    .await?
    .join(" ");

    let messages = JIEBA
        .cut(&messages, true)
        .into_iter()
        .map(|t| t.word)
        .filter(|s| s.chars().count() > 1)
        .join(" ");

    let wc_cli = crate::config::read_config().wordcloud_cli_path.clone();
    let wc_cli = validate_wordcloud_cli_path(&wc_cli)
        .map_err(|e| anyhow::anyhow!("wordcloud_cli 路径不合法: {e}"))?;
    let mask_path = path.join("mask.jpg");
    let stop_word_path = path.join("stopword.txt");
    let fontfile_path = path.join("font.otf");

    let mut command = tokio::process::Command::new(wc_cli);

    command
        .args(["--background", "white"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if mask_path.exists() {
        command.arg("--mask").arg(mask_path);
    }

    if stop_word_path.exists() {
        command.arg("--stopwords").arg(stop_word_path);
    }

    if fontfile_path.exists() {
        command.arg("--fontfile").arg(fontfile_path);
    }

    let mut child = command.spawn()?;

    let output = match timeout(Duration::from_secs(60), async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取 wordcloud_cli 的 stdin"))?;
        stdin.write_all(messages.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let output = child.wait_with_output().await?;
        anyhow::Ok(output)
    })
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("wordcloud_cli 执行超时"),
    };

    if !output.status.success() {
        anyhow::bail!(
            "wordcloud_cli failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(output.stdout)
}

/// 校验 wordcloud_cli 路径：必须是绝对路径且文件存在。
fn validate_wordcloud_cli_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        anyhow::bail!("必须是绝对路径");
    }
    if !p.is_file() {
        anyhow::bail!("wordcloud_cli 必须是可执行文件: {}", p.display());
    }
    Ok(p.to_path_buf())
}
