//! 官方事件 payload → 官方事件 JSON（自有形状，openid 字符串身份）。
//!
//! 输出喂给 [`crate::event::MsgEvent::de`]；字段约定见 event.rs。

use serde_json::{Value, json};

use crate::Context;
use crate::passive::{Conversation, conversation_key};

pub fn translate(ctx: &Context, event_type: &str, payload: &Value) -> Option<Value> {
    match event_type {
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => translate_group(ctx, payload),
        "C2C_MESSAGE_CREATE" => translate_private(ctx, payload),
        _ => None,
    }
}

fn translate_group(ctx: &Context, payload: &Value) -> Option<Value> {
    let group_openid = payload["group_openid"].as_str()?;
    let message_id = payload["id"].as_str()?;
    let author = &payload["author"];
    let member_openid = author["member_openid"].as_str()?;
    let nickname = author["username"].as_str().unwrap_or_default();
    let card = author["member_nickname"]
        .as_str()
        .filter(|name| !name.is_empty());
    let avatar = author["avatar"].as_str();

    // 官方没有按需查询成员的接口，事件流是唯一来源，路过即入缓存。
    ctx.members
        .update(group_openid, member_openid, nickname, card, avatar);
    ctx.inbound.register(
        &conversation_key(Conversation::Group, group_openid),
        message_id,
    );

    let text = strip_mentions(payload["content"].as_str().unwrap_or_default());
    tracing::info!(
        group = group_openid,
        user = member_openid,
        text = %text.chars().take(50).collect::<String>(),
        "收到群消息事件"
    );
    let segments = build_segments(&text, &payload["attachments"]);

    Some(json!({
        "time": timestamp_to_epoch(payload["timestamp"].as_str()),
        "group_openid": group_openid,
        "user_openid": member_openid,
        "nickname": nickname,
        "card": card,
        "avatar": avatar,
        "message_id": message_id,
        "message": segments,
    }))
}

fn translate_private(ctx: &Context, payload: &Value) -> Option<Value> {
    let message_id = payload["id"].as_str()?;
    let author = &payload["author"];
    let user_openid = author["user_openid"]
        .as_str()
        .or_else(|| author["id"].as_str())?;
    let nickname = author["username"].as_str().unwrap_or_default();
    let avatar = author["avatar"].as_str();

    ctx.inbound.register(
        &conversation_key(Conversation::User, user_openid),
        message_id,
    );

    let text = payload["content"].as_str().unwrap_or_default().to_owned();
    tracing::info!(
        user = user_openid,
        text = %text.chars().take(50).collect::<String>(),
        "收到单聊消息事件"
    );
    let segments = build_segments(&text, &payload["attachments"]);

    Some(json!({
        "time": timestamp_to_epoch(payload["timestamp"].as_str()),
        "user_openid": user_openid,
        "nickname": nickname,
        "avatar": avatar,
        "message_id": message_id,
        "message": segments,
    }))
}

fn build_segments(text: &str, attachments: &Value) -> Vec<Value> {
    let mut segments = Vec::new();
    if !text.is_empty() {
        segments.push(json!({ "type": "text", "data": { "text": text } }));
    }
    for attachment in attachments.as_array().unwrap_or(&Vec::new()) {
        let is_image = attachment["content_type"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("image/"));
        let url = attachment["url"].as_str();
        if let (true, Some(url)) = (is_image, url) {
            segments.push(json!({ "type": "image", "data": { "file": url, "url": url } }));
        }
    }
    segments
}

/// 去掉 `<@openid>` / `<@!openid>` 提及标记并折叠多余空白。
/// 机器人被 @ 的文本里只剩标记本身，去掉后可能得到空消息。
fn strip_mentions(content: &str) -> String {
    let mut cleaned = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        cleaned.push_str(&rest[..start]);
        let tail = &rest[start..];
        match tail.find('>') {
            Some(end) => rest = &tail[end + 1..],
            None => {
                cleaned.push_str(tail);
                rest = "";
                break;
            }
        }
    }
    cleaned.push_str(rest);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn timestamp_to_epoch(timestamp: Option<&str>) -> i64 {
    timestamp
        .and_then(|raw| {
            time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
        })
        .map(|parsed| parsed.unix_timestamp())
        .unwrap_or_else(|| time::OffsetDateTime::now_utc().unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_event() -> Value {
        json!({
            "id": "MSG-001",
            "group_openid": "GROUP-ABC",
            "content": "<@!BOTID> /B话榜 日",
            "timestamp": "2026-11-20T12:00:00+08:00",
            "author": {
                "member_openid": "MEMBER-1",
                "username": "张三",
                "member_nickname": "三哥",
                "avatar": "https://avatar.example/1.png",
            },
            "attachments": [
                {
                    "content_type": "image/jpeg",
                    "url": "https://media.example/a.jpg",
                    "filename": "a.jpg",
                },
                {
                    "content_type": "file",
                    "url": "https://media.example/b.bin",
                },
            ],
        })
    }

    #[test]
    fn 群事件翻译携带openid身份() {
        let ctx = Context::for_tests();
        let event = translate(&ctx, "GROUP_AT_MESSAGE_CREATE", &group_event()).unwrap();

        assert_eq!(event["group_openid"], "GROUP-ABC");
        assert_eq!(event["user_openid"], "MEMBER-1");
        assert_eq!(event["message_id"], "MSG-001");
        assert_eq!(event["nickname"], "张三");
        assert_eq!(event["card"], "三哥");
        assert_eq!(event["time"], 1_795_147_200_i64);
        assert_eq!(event["message"][0]["data"]["text"], "/B话榜 日");
        // 非图片附件不进消息段。
        assert_eq!(event["message"].as_array().unwrap().len(), 2);
        assert_eq!(
            event["message"][1]["data"]["url"],
            "https://media.example/a.jpg"
        );
    }

    #[test]
    fn 群事件顺手更新成员缓存与被动槽位() {
        let ctx = Context::for_tests();
        translate(&ctx, "GROUP_AT_MESSAGE_CREATE", &group_event());
        let info = ctx.members.member_info("GROUP-ABC", "MEMBER-1").unwrap();
        assert_eq!(info["nickname"], "张三");
        assert_eq!(
            ctx.inbound
                .take_reply_slot(&conversation_key(Conversation::Group, "GROUP-ABC"))
                .unwrap()
                .0,
            "MSG-001"
        );
    }

    #[test]
    fn 单聊事件无群字段() {
        let ctx = Context::for_tests();
        let payload = json!({
            "id": "C2C-1",
            "content": "你好",
            "author": { "user_openid": "USER-9", "username": "李四" },
        });
        let event = translate(&ctx, "C2C_MESSAGE_CREATE", &payload).unwrap();
        assert_eq!(event["user_openid"], "USER-9");
        assert!(event.get("group_openid").is_none());
        assert_eq!(event["message"][0]["data"]["text"], "你好");
    }

    #[test]
    fn 提及标记剥离() {
        assert_eq!(strip_mentions("<@!abc> 文字"), "文字");
        assert_eq!(strip_mentions("a <@abc> b"), "a b");
        assert_eq!(strip_mentions("<@!abc>"), "");
        assert_eq!(strip_mentions("无标记 < 保持"), "无标记 < 保持");
    }
}
