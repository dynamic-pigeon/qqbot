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

    let raw_text = strip_mentions(payload["content"].as_str().unwrap_or_default());
    // 引用回复时顶层 content 只剩空格，用户真正输入的正文与随信图片都
    // 落在 msg_elements 里；被引用消息（message_type 102/103）是别人的
    // 内容，不并入正文。
    if payload["msg_elements"]
        .as_array()
        .is_some_and(|list| !list.is_empty())
    {
        tracing::debug!("携带 msg_elements 的群消息，原始载荷: {payload}");
    }
    let (text, element_attachments) = if raw_text.is_empty() {
        text_from_msg_elements(&payload["msg_elements"])
    } else {
        (raw_text, Vec::new())
    };
    let mut attachments: Vec<&Value> = payload["attachments"]
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_default();
    attachments.extend(element_attachments);
    let mut segments = Vec::new();
    if let Some(reply) = reply_segment(payload) {
        segments.push(reply);
    }
    let built = build_segments(&text, &attachments);
    if built.is_empty() && segments.is_empty() {
        tracing::debug!("群消息无可解析内容，原始载荷: {payload}");
    }
    segments.extend(built);

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

/// 引用回复转 reply 段：被引用消息（msg_elements 里 message_type=103 的
/// 元素）的文本与图片直链内联在段数据里——平台没有按 id 回查消息的
/// API，下游（图库回复取图）直接读段数据；id 取 message_scene.ext 的
/// ref_msg_idx。
fn reply_segment(payload: &Value) -> Option<Value> {
    let quoted = payload["msg_elements"]
        .as_array()?
        .iter()
        .find(|element| element["message_type"].as_i64() == Some(103))?;
    let images: Vec<&str> = quoted["attachments"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|attachment| {
                    attachment["content_type"]
                        .as_str()
                        .is_some_and(|mime| mime.starts_with("image/"))
                })
                .filter_map(|attachment| attachment["url"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let id = payload["message_scene"]["ext"].as_array().and_then(|ext| {
        ext.iter().find_map(|entry| {
            entry
                .as_str()
                .and_then(|kv| kv.strip_prefix("ref_msg_idx="))
        })
    });
    Some(json!({
        "type": "reply",
        "data": {
            "id": id,
            "text": quoted["content"].as_str().unwrap_or_default(),
            "images": images,
        }
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

    let raw_text = payload["content"].as_str().unwrap_or_default().to_owned();
    let (text, element_attachments) = if raw_text.trim().is_empty() {
        text_from_msg_elements(&payload["msg_elements"])
    } else {
        (raw_text, Vec::new())
    };
    let mut attachments: Vec<&Value> = payload["attachments"]
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_default();
    attachments.extend(element_attachments);
    let segments = build_segments(&text, &attachments);
    if segments.is_empty() {
        tracing::debug!("单聊消息无可解析内容，原始载荷: {payload}");
    }

    Some(json!({
        "time": timestamp_to_epoch(payload["timestamp"].as_str()),
        "user_openid": user_openid,
        "nickname": nickname,
        "avatar": avatar,
        "message_id": message_id,
        "message": segments,
    }))
}

/// 从 msg_elements 提取用户输入的正文与随信附件。message_type：
/// 0=普通文本，102=聊天记录，103=引用消息；只取 0——102/103 是被引用
/// 的他人内容，并入会污染消息统计与图库。
fn text_from_msg_elements(elements: &Value) -> (String, Vec<&Value>) {
    let mut parts: Vec<String> = Vec::new();
    let mut attachments: Vec<&Value> = Vec::new();
    for element in elements.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if element["message_type"].as_i64() != Some(0) {
            continue;
        }
        let text = strip_mentions(element["content"].as_str().unwrap_or_default());
        if !text.is_empty() {
            parts.push(text);
        }
        if let Some(list) = element["attachments"].as_array() {
            attachments.extend(list.iter());
        }
    }
    (parts.join(" "), attachments)
}

fn build_segments(text: &str, attachments: &[&Value]) -> Vec<Value> {
    let mut segments = Vec::new();
    if !text.is_empty() {
        segments.push(json!({ "type": "text", "data": { "text": text } }));
    }
    for attachment in attachments {
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

    #[test]
    fn 引用回复生成reply段并保留正文() {
        let ctx = Context::for_tests();
        let payload = json!({
            "id": "MSG-R1",
            "group_openid": "GROUP-ABC",
            "content": " 666",
            "author": { "member_openid": "MEMBER-1", "username": "张三" },
            "message_scene": { "ext": ["ref_msg_idx=REFIDX_QUOTED", "msg_idx=REFIDX_SELF"] },
            "msg_elements": [
                {
                    "message_type": 103,
                    "content": "6",
                    "attachments": [
                        {
                            "content_type": "image/jpeg",
                            "url": "https://multimedia.nt.qq.com.cn/download?a=1",
                        },
                        {
                            "content_type": "image/jpeg",
                            "url": "https://multimedia.nt.qq.com.cn/download?a=2",
                        },
                    ],
                },
            ],
        });
        let event = translate(&ctx, "GROUP_MESSAGE_CREATE", &payload).unwrap();
        // 引用独立成段：被引文本、图片直链、ref_msg_idx 全部内联。
        let reply = &event["message"][0];
        assert_eq!(reply["type"], "reply");
        assert_eq!(reply["data"]["id"], "REFIDX_QUOTED");
        assert_eq!(reply["data"]["text"], "6");
        assert_eq!(reply["data"]["images"].as_array().unwrap().len(), 2);
        // 用户正文照常成段，被引图片不进正文图片段。
        assert_eq!(event["message"][1]["type"], "text");
        assert_eq!(event["message"][1]["data"]["text"], "666");
        assert_eq!(event["message"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn 纯引用不打字只有reply段() {
        let ctx = Context::for_tests();
        let payload = json!({
            "id": "MSG-R2",
            "group_openid": "GROUP-ABC",
            "content": " ",
            "author": { "member_openid": "MEMBER-1", "username": "张三" },
            "message_scene": { "ext": ["ref_msg_idx=REFIDX_Q"] },
            "msg_elements": [
                {
                    "message_type": 103,
                    "content": "原消息",
                    "attachments": [
                        { "content_type": "image/png", "url": "https://media.example/q.png" },
                    ],
                },
            ],
        });
        let event = translate(&ctx, "GROUP_MESSAGE_CREATE", &payload).unwrap();
        let segments = event["message"].as_array().unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0]["type"], "reply");
        assert_eq!(
            segments[0]["data"]["images"][0],
            "https://media.example/q.png"
        );
    }
}
