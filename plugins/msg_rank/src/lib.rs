use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use kovi::{Message, PluginBuilder as plugin, event::GroupMsgEvent, log, tokio};

mod config;
mod db;
mod msg_rank;
mod ocr;
mod word_cloud;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
});

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    let path = Arc::new(bot.get_data_path());

    let config_path = path.join("config.json");
    config::init_config(config_path).await.unwrap();

    let db_path = path.join("msg.db");
    if !db_path.exists() {
        tokio::fs::create_dir_all(db_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::File::create(&db_path).await.unwrap();
    }

    db::init_db(&db_path).await.unwrap();

    plugin::on_group_msg(add_msg);
    word_cloud::init().await.unwrap();
    msg_rank::init().await.unwrap();
}

async fn add_msg(event: Arc<GroupMsgEvent>) {
    let group = event.group_id;
    let text = if config::read_config().await.notify_group.contains(&group) {
        &get_text(&event.message).await
    } else {
        // 如果不在监控的群里，就不进行OCR，直接返回文本内容
        event.borrow_text().unwrap_or_default()
    };
    if let Err(e) = db::add_msg(event.group_id, event.user_id, text).await {
        log::error!("添加消息失败: {}", e);
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
