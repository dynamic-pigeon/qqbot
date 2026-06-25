use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use kovi::{Message, PluginBuilder as plugin, tokio};
use kovi_onebot::{EventRegistrar as _, event::GroupMsgEvent};

#[macro_use]
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

    let text = if config::read_config().notify_group.contains(&group) {
        &get_text(&event.message).await
    } else {
        // 如果不在监控的群里，就不进行OCR，直接返回文本内容
        event.borrow_text().unwrap_or_default()
    };
    if let Err(e) = db::add_msg(event.group_id, event.user_id, text).await {
        tracing::error!("添加消息失败: {}", e);
    }
}

async fn get_text(msg: &Message) -> String {
    let mut res = String::new();

    // 在没有图片的时候只会有栈分配，有图片的时候才会有堆分配，所以这里不需要担心性能问题
    let mut tasks = Vec::new();

    // 先把文本内容和图片URL提取出来，文本内容直接拼接，图片URL则交给OCR任务处理
    // 顺序不重要
    for seg in msg.iter() {
        if !res.is_empty() {
            res.push(' ');
        }
        match seg.kind.as_str() {
            "text" => res.push_str(seg.data["text"].as_str().unwrap()),
            "image" => {
                let url = seg.data["url"].as_str().unwrap().to_string();
                let task = kovi::spawn(async move { ocr::ocr(&url).await });
                tasks.push(task);
            }
            _ => {}
        }
    }

    for task in tasks {
        match task.await {
            Ok(Ok(text)) => {
                if !text.is_empty() {
                    res.push_str(&text);
                }
            }
            Ok(Err(e)) => {
                tracing::error!("OCR 失败: {}", e);
            }
            Err(e) => {
                tracing::error!("OCR 任务失败: {}", e);
            }
        }
    }

    res
}
