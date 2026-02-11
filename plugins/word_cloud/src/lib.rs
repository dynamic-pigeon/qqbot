use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
};

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin, RuntimeBot, chrono,
    log::{self, debug, info},
    tokio::{
        self,
        io::AsyncWriteExt,
        sync::{RwLock, RwLockReadGuard},
    },
};

mod ocr;

static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();

#[kovi::plugin]
async fn main() {
    help_msg::register_help(
        "wordcloud",
        "启用或禁用词云功能（管理员专用命令）",
        "/wordcloud enable - 启用词云功能\n/wordcloud disable - 禁用词云功能",
    )
    .await;

    let bot = plugin::get_runtime_bot();
    let path = bot.get_data_path();

    let config_path = path.join("config.json");
    let mut config: Config = kovi::utils::load_json_data(Default::default(), &config_path).unwrap();
    config.path = config_path;

    debug!("config: {:?}", config);

    CONFIG.get_or_init(|| RwLock::new(config));

    let db_path = path.join("word_cloud.db");

    if !db_path.exists() {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::File::create(&db_path).unwrap();
    }

    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(3)
        .connect(db_path.to_str().unwrap())
        .await
        .unwrap();

    let db = Arc::new(db);

    init(&db).await;

    let db_clone = Arc::clone(&db);

    let path = Arc::new(path);

    let bot_clone = Arc::clone(&bot);

    plugin::cron("0 21 * * *", move || {
        let path = Arc::clone(&path);
        let bot = Arc::clone(&bot);
        let db = Arc::clone(&db);
        async move {
            let config = read_config().await;
            let notify_group = &config.notify_group;

            for &group_id in notify_group {
                let bot = Arc::clone(&bot);
                let path = Arc::clone(&path);
                let db = Arc::clone(&db);
                kovi::spawn(async move {
                    send_word_cloud(&bot, group_id, &path, &db).await;
                });
            }
            drop(config);
            remove_before(&db, chrono::Utc::now() - chrono::Duration::days(7)).await;
        }
    })
    .unwrap();

    plugin::on_group_msg(move |event| {
        let db = Arc::clone(&db_clone);
        let bot = Arc::clone(&bot_clone);
        async move {
            let group_id = event.group_id;

            let text = event.borrow_text().unwrap_or_default();
            let msg = text.trim();

            if let Some(cmd) = msg.strip_prefix("/wordcloud ") {
                if !bot.get_all_admin().unwrap().contains(&event.sender.user_id) {
                    event.reply("❌ 管理员专用命令，普通用户无法使用");
                    return;
                }
                let cmd = cmd.trim();
                match exe_cmd(cmd, group_id).await {
                    Ok(response) => {
                        event.reply(format!("✅ {}", response));
                    }
                    Err(e) => {
                        log::error!("{e}");
                        event.reply(format!("❌ 命令执行失败: {}", e));
                    }
                }
                return;
            }

            if !read_config().await.notify_group.contains(&group_id) {
                return;
            }

            let msg = get_text(&event.message).await;

            if msg.is_empty() {
                return;
            }

            add_msg(&db, group_id, &msg).await;
        }
    });
}

async fn exe_cmd(cmd: &str, group_id: i64) -> Result<&str> {
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
        _ => {
            anyhow::bail!("未知命令: {}", cmd);
        }
    }
}

async fn get_text(msg: &Message) -> String {
    let mut res = String::new();

    for seg in msg.iter() {
        if !res.is_empty() {
            res.push(' ');
        }
        match seg.type_.as_str() {
            "text" => res.push_str(seg.data["text"].as_str().unwrap()),
            "image" => match ocr::ocr(seg.data["url"].as_str().unwrap()).await {
                Ok(tx) => {
                    res.push_str(&tx);
                }
                Err(e) => {
                    log::error!("ocr failed: {}", e);
                }
            },
            _ => {}
        }
    }

    res
}

async fn init(db: &sqlx::SqlitePool) {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS group_message 
    (group_id INTEGER, message TEXT, time TEXT)
        "#,
    )
    .execute(db)
    .await
    .unwrap();
}

async fn add_msg(db: &sqlx::SqlitePool, group_id: i64, message: &str) {
    sqlx::query(
        r#"
        INSERT INTO group_message (group_id, message, time) VALUES (?, ?, ?)
        "#,
    )
    .bind(group_id)
    .bind(message)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(db)
    .await
    .unwrap();
}

async fn make_word_cloud(path: &Path, notify_group: i64, db: &sqlx::SqlitePool) -> Result<Vec<u8>> {
    let end_time = chrono::Utc::now();
    let start_time = end_time - chrono::Duration::days(1) - chrono::Duration::minutes(10);

    let messages = select_from_range(db, notify_group, start_time, end_time)
        .await?
        .join(" ");

    let msg = jieba_rs::Jieba::new();
    let messages = msg
        .cut(&messages, true)
        .into_iter()
        .filter(|s| s.chars().count() > 1)
        .collect::<Vec<_>>()
        .join(" ");

    let wc_cli = read_config().await.wordcloud_cli_path.clone();
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

async fn send_word_cloud(bot: &RuntimeBot, group_id: i64, path: &Path, db: &sqlx::SqlitePool) {
    let image = match make_word_cloud(path, group_id, db).await {
        Ok(image) if !image.is_empty() => image,
        Ok(image) => {
            assert!(image.is_empty());
            info!("word cloud is empty, group_id: {}", group_id);
            return;
        }
        Err(e) => {
            log::error!("make word cloud failed: {}, group_id: {}", e, group_id);
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
    bot.send_group_msg(
        group_id,
        Message::new().add_text("今日词云").add_image(&image),
    );
}

async fn select_from_range(
    db: &sqlx::SqlitePool,
    group_id: i64,
    start_time: chrono::DateTime<chrono::Utc>,
    end_time: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<String>> {
    let result: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT message FROM group_message WHERE group_id = ? AND time BETWEEN ? AND ?
        "#,
    )
    .bind(group_id)
    .bind(start_time.to_rfc3339())
    .bind(end_time.to_rfc3339())
    .fetch_all(db)
    .await?;

    Ok(result.into_iter().map(|(msg,)| msg).collect())
}

async fn remove_before(db: &sqlx::SqlitePool, time: chrono::DateTime<chrono::Utc>) {
    sqlx::query(
        r#"
        DELETE FROM group_message WHERE time < ?
        "#,
    )
    .bind(time.to_rfc3339())
    .execute(db)
    .await
    .unwrap();
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
struct Config {
    pub wordcloud_cli_path: String,
    pub notify_group: Vec<i64>,
    #[serde(rename = "SecretId")]
    pub secret_id: String,
    #[serde(rename = "SecretKey")]
    pub secret_key: String,
    #[serde(skip)]
    pub path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wordcloud_cli_path: "wordcloud_cli".to_string(),
            notify_group: vec![],
            secret_id: "".to_string(),
            secret_key: "".to_string(),
            path: PathBuf::new(),
        }
    }
}

#[inline(always)]
async fn modify_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    let cfg = CONFIG.get().unwrap();
    let mut config = cfg.write().await;
    f(&mut config);
    write_config(&mut config).await
}

#[inline(always)]
async fn read_config<'a>() -> RwLockReadGuard<'a, Config> {
    let cfg = CONFIG.get().unwrap();
    cfg.read().await
}

async fn write_config(config: &mut Config) -> Result<()> {
    let config_path = &config.path;
    match kovi::utils::save_json_data(&*config, config_path) {
        Err(e) => {
            anyhow::bail!("保存配置文件失败: {}", e);
        }
        Ok(_) => Ok(()),
    }
}
