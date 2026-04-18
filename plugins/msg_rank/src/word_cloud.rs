use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use itertools::Itertools as _;
use kovi::{
    Message, PluginBuilder as plugin, RuntimeBot,
    event::GroupMsgEvent,
    tokio::{self, io::AsyncWriteExt as _},
};
use tracing::{self, info};

use crate::config::{modify_config, read_config};

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

    if !bot.get_all_admin().unwrap().contains(&event.user_id) {
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
            bot.send_private_msg(
                bot.get_main_admin().unwrap(),
                format!("make word cloud failed: {}, group_id: {}", e, group_id),
            );
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
    let end_time = chrono::Local::now();
    let start_time = end_time - duration;

    let messages = crate::db::select_from_time_range(
        notify_group,
        start_time.timestamp(),
        end_time.timestamp(),
    )
    .await?
    .join(" ");

    let msg = jieba_rs::Jieba::new();
    let messages = msg
        .cut(&messages, true)
        .into_iter()
        .filter(|s| s.chars().count() > 1)
        .join(" ");

    let wc_cli = crate::config::read_config().wordcloud_cli_path.clone();
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

    child
        .stdin
        .take()
        .unwrap()
        .write_all(messages.as_bytes())
        .await?;

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        anyhow::bail!(
            "wordcloud_cli failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(output.stdout)
}
