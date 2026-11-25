//! 官方事件 payload → 官方事件 JSON（自有形状，openid 字符串身份）。
//!
//! 输出喂给 [`crate::event::MsgEvent::de`]；字段约定见 event.rs。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kovi::message::Segment;
use serde_json::{Value, json};

use crate::ChatScope;
use crate::Context;

pub fn translate(ctx: &Context, event_type: &str, payload: &Value) -> Option<Value> {
    let message_id = payload["id"].as_str()?;
    if !ctx.seen.insert(message_id) {
        tracing::debug!("重复推送的消息，丢弃: {message_id}");
        return None;
    }
    match event_type {
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
            translate_group(ctx, payload, message_id)
        }
        "C2C_MESSAGE_CREATE" => translate_private(ctx, payload, message_id),
        _ => None,
    }
}

/// 入站去重：官方会以相同 msg_id 概率性重推同一条消息（event.md），
/// 短窗内重复直接丢弃，避免命令执行两次、统计重复计数。
pub(crate) struct SeenMessages {
    inner: std::sync::Mutex<HashMap<String, Instant>>,
}

/// 官方 id 全局唯一，窗口只需盖住重推间隔；顺带周期性清理过期项。
const DEDUP_WINDOW: Duration = Duration::from_secs(10 * 60);

impl SeenMessages {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 首见记录并返回 true；窗口内重复返回 false。
    pub(crate) fn insert(&self, id: &str) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        map.retain(|_, seen| now.duration_since(*seen) < DEDUP_WINDOW);
        if map.contains_key(id) {
            return false;
        }
        map.insert(id.to_owned(), now);
        true
    }
}

fn translate_group(ctx: &Context, payload: &Value, message_id: &str) -> Option<Value> {
    let group_openid = payload["group_openid"].as_str()?;
    let author = &payload["author"];
    let member_openid = author["member_openid"].as_str()?;
    let nickname = author["username"].as_str().unwrap_or_default();
    let card = author["member_nickname"]
        .as_str()
        .filter(|name| !name.is_empty());
    let avatar = author["avatar"].as_str();

    ctx.members
        .update(group_openid, member_openid, nickname, card, avatar);
    ctx.inbound
        .register(&ChatScope::Group(group_openid.to_owned()), message_id);

    if payload["msg_elements"]
        .as_array()
        .is_some_and(|list| !list.is_empty())
    {
        tracing::debug!("携带 msg_elements 的群消息，原始载荷: {payload}");
    }

    Some(json!({
        "time": timestamp_to_epoch(payload["timestamp"].as_str()),
        "group_openid": group_openid,
        "user_openid": member_openid,
        "nickname": nickname,
        "card": card,
        "avatar": avatar,
        "message_id": message_id,
        "message": message_segments(payload),
    }))
}

fn translate_private(ctx: &Context, payload: &Value, message_id: &str) -> Option<Value> {
    let author = &payload["author"];
    let user_openid = author["user_openid"]
        .as_str()
        .or_else(|| author["id"].as_str())?;
    ctx.inbound
        .register(&ChatScope::User(user_openid.to_owned()), message_id);

    Some(json!({
        "time": timestamp_to_epoch(payload["timestamp"].as_str()),
        "user_openid": user_openid,
        "nickname": author["username"].as_str().unwrap_or_default(),
        "avatar": author["avatar"].as_str(),
        "message_id": message_id,
        "message": message_segments(payload),
    }))
}

/// 一条消息的消息段：引用、正文、图片。
fn message_segments(payload: &Value) -> Vec<Segment> {
    let raw_text = sanitize_content(payload["content"].as_str().unwrap_or_default());
    // 顶层 content 非空时就是用户正文；为空时回退读 msg_elements 的正文元素。
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

    let mut segments = reply_segment(payload).into_iter().collect::<Vec<_>>();
    segments.extend(build_segments(&text, &attachments));
    if segments.is_empty() {
        tracing::debug!("消息无可解析内容，原始载荷: {payload}");
    }
    segments
}

/// 引用回复转 reply 段：data 是官方 msg_elements 首个元素原样（引用时它就是
/// 被引消息，含 content / attachments / ark_data 与自身的 msg_idx），不做
/// 字段重解释。触发信号取官方自己的标记：元素 message_type=103 或 ext 带
/// ref_msg_idx。
fn reply_segment(payload: &Value) -> Option<Segment> {
    let quoted = payload["msg_elements"].as_array()?.first()?;
    let is_quote = quoted["message_type"].as_i64() == Some(103)
        || has_ref_msg_idx(payload["message_scene"]["ext"].as_array());
    is_quote.then(|| Segment::new("reply", quoted.clone()))
}

/// message_scene.ext 是 `key=value` 字符串数组；`ref_msg_idx` 取值为空的
/// 条目不算引用。
fn has_ref_msg_idx(ext: Option<&Vec<Value>>) -> bool {
    let Some(entries) = ext else {
        return false;
    };
    entries.iter().any(|entry| {
        entry
            .as_str()
            .and_then(|entry| entry.split_once('='))
            .is_some_and(|(key, value)| key.trim() == "ref_msg_idx" && !value.trim().is_empty())
    })
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
        let text = sanitize_content(element["content"].as_str().unwrap_or_default());
        if !text.is_empty() {
            parts.push(text);
        }
        if let Some(list) = element["attachments"].as_array() {
            attachments.extend(list.iter());
        }
    }
    (parts.join(" "), attachments)
}

fn build_segments(text: &str, attachments: &[&Value]) -> Vec<Segment> {
    let mut segments = Vec::new();
    if !text.is_empty() {
        segments.push(Segment::new("text", json!({ "text": text })));
    }
    for attachment in attachments {
        let is_image = attachment["content_type"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("image/"));
        let url = attachment["url"].as_str();
        if let (true, Some(url)) = (is_image, url) {
            segments.push(Segment::new("image", json!({ "file": url, "url": url })));
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

/// 官方对合并转发消息的文本化固定以此开头，「=== 消息 n ===」之后是
/// 被转发者的内容，整条不是用户输入。
const CHAT_RECORD_PREFIX: &str = "[群聊的聊天记录]";

/// 官方顶层 content 是整条消息的文本化产物：@ 提及、表情、合并转发
/// 都会序列化成标记文本。这里只留用户敲的字——提及与表情、内部标记
/// 剥除（形态同 qqbot-nodejs 的 content-sanitizer 与 text-parsing），
/// 合并转发摘要整条置空。剥完只剩空白即无正文。
fn sanitize_content(content: &str) -> String {
    let text = strip_platform_markers(&strip_mentions(content));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.starts_with(CHAT_RECORD_PREFIX) {
        return String::new();
    }
    text
}

fn strip_platform_markers(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['<', '[']) {
        cleaned.push_str(&rest[..start]);
        let tail = &rest[start..];
        match strip_marker(tail) {
            Some(after) => rest = after,
            None => {
                // 手打的 < 或 [，只消费一个字符，后面的真标记照常识别。
                let ch = tail.chars().next().expect("find 命中处必有字符");
                cleaned.push(ch);
                rest = &tail[ch.len_utf8()..];
            }
        }
    }
    cleaned.push_str(rest);
    cleaned
}

/// `tail` 开头是平台标记时返回标记之后的剩余部分。
fn strip_marker(tail: &str) -> Option<&str> {
    if tail.starts_with("<faceType=") {
        return face_tag_len(tail);
    }
    if tail.starts_with("[<face,") {
        return old_face_tag_len(tail);
    }
    internal_marker_len(tail)
}

/// 新版表情标记：`<faceType=6,faceId="0",ext="base64">`。
fn face_tag_len(tail: &str) -> Option<&str> {
    let rest = &tail["<faceType=".len()..];
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits == 0 {
        return None;
    }
    let rest = rest[digits..].strip_prefix(",faceId=\"")?;
    let id_end = rest.find('"')?;
    let rest = rest[id_end + 1..].strip_prefix(",ext=\"")?;
    let ext_end = rest.find('"')?;
    rest[ext_end + 1..].strip_prefix('>')
}

/// 旧版表情标记：`[<face,id=6/>]` / `[<face,id=6>]`。
fn old_face_tag_len(tail: &str) -> Option<&str> {
    let rest = tail.strip_prefix("[<face,id=")?;
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits == 0 {
        return None;
    }
    let rest = &rest[digits..];
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let rest = rest.strip_prefix('>')?;
    rest.strip_prefix(']')
}

/// 平台内部标记：`[internal:…]` / `[debug:…]` / `[system:…]`，
/// 关键字大小写不敏感，冒号可省。
fn internal_marker_len(tail: &str) -> Option<&str> {
    let rest = tail.strip_prefix('[')?;
    for keyword in ["internal", "debug", "system"] {
        // 字节层比较：关键字长度若切在多字节字符中间，必然不是关键字。
        let head = &rest.as_bytes()[..keyword.len().min(rest.len())];
        if head.eq_ignore_ascii_case(keyword.as_bytes()) {
            let after_keyword = &rest[keyword.len()..];
            let body = after_keyword.strip_prefix(':').unwrap_or(after_keyword);
            let ws = body
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(body.len());
            let body = &body[ws..];
            let end = body.find(']')?;
            return Some(&body[end + 1..]);
        }
    }
    None
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

    /// 走生产同款反序列化路径取段列表（event.rs 的 from_value 链）。
    fn segments_of(event: &Value) -> Vec<Segment> {
        serde_json::from_value(event["message"].clone()).unwrap()
    }

    fn group_event() -> Value {
        json!({
            "id": "MSG-001",
            "group_openid": "GROUP-ABC",
            "content": "<@!BOTID> /B话榜 日",
            "timestamp": "2026-11-20T12:00:00+08:00",
            "author": { "member_openid": "MEMBER-1", "username": "张三", "member_nickname": "三哥" },
            "attachments": [
                { "content_type": "image/jpeg", "url": "https://media.example/a.jpg" },
                { "content_type": "file", "url": "https://media.example/b.bin" },
            ],
        })
    }

    /// 带引用的群消息：`content` 是用户正文，`ext` 是 message_scene.ext，
    /// `elements` 是 msg_elements。
    fn quote_event(content: &str, ext: Value, elements: Value) -> Value {
        json!({
            "id": "MSG-R",
            "group_openid": "GROUP-ABC",
            "content": content,
            "author": { "member_openid": "MEMBER-1", "username": "张三" },
            "message_scene": { "ext": ext },
            "msg_elements": elements,
        })
    }

    /// 被引的图片消息：引用时官方把它内联在 msg_elements 首位。
    fn quoted_image() -> Value {
        json!({
            "message_type": 103,
            "msg_idx": "REFIDX",
            "content": "原消息",
            "attachments": [{ "content_type": "image/png", "url": "https://media.example/q.png" }],
        })
    }

    #[test]
    fn 重复推送的消息只翻译一次() {
        let ctx = Context::for_tests();
        let event = group_event();
        assert!(translate(&ctx, "GROUP_AT_MESSAGE_CREATE", &event).is_some());
        assert!(translate(&ctx, "GROUP_AT_MESSAGE_CREATE", &event).is_none());
    }

    #[test]
    fn 群事件翻译身份并登记成员与被动槽位() {
        let ctx = Context::for_tests();
        let event = translate(&ctx, "GROUP_AT_MESSAGE_CREATE", &group_event()).unwrap();

        assert_eq!(event["group_openid"], "GROUP-ABC");
        assert_eq!(event["user_openid"], "MEMBER-1");
        assert_eq!(event["nickname"], "张三");
        assert_eq!(event["card"], "三哥");
        assert_eq!(event["time"], 1_795_147_200_i64);
        // 只有图片附件进消息段。
        let segments = segments_of(&event);
        assert_eq!(segments[0].kind, "text");
        assert_eq!(segments[0].data["text"], "/B话榜 日");
        assert_eq!(segments[1].kind, "image");
        assert_eq!(segments.len(), 2);

        let info = ctx.members.member_info("GROUP-ABC", "MEMBER-1").unwrap();
        assert_eq!(info["nickname"], "张三");
        let scope = ChatScope::Group("GROUP-ABC".to_owned());
        assert_eq!(ctx.inbound.take_reply_slot(&scope).unwrap().0, "MSG-001");
    }

    #[test]
    fn 单聊事件身份与引用段() {
        let ctx = Context::for_tests();
        let payload = json!({
            "id": "C2C-1",
            "content": " 666",
            "author": { "user_openid": "USER-9", "username": "李四" },
            "message_scene": { "ext": ["ref_msg_idx=REFIDX"] },
            "msg_elements": [quoted_image()],
        });
        let event = translate(&ctx, "C2C_MESSAGE_CREATE", &payload).unwrap();

        assert_eq!(event["user_openid"], "USER-9");
        assert!(event.get("group_openid").is_none());
        let segments = segments_of(&event);
        assert_eq!(segments[0].kind, "reply");
        assert_eq!(segments[1].kind, "text");
        assert_eq!(segments[1].data["text"], "666");
        let scope = ChatScope::User("USER-9".to_owned());
        assert_eq!(ctx.inbound.take_reply_slot(&scope).unwrap().0, "C2C-1");
    }

    #[test]
    fn 正文清理只留用户敲的字() {
        let cases = [
            // @ 提及。
            ("<@!abc> 文字", "文字"),
            ("a <@abc> b", "a b"),
            ("<@!abc>", ""),
            // 表情：旧版 [<face,id=6/>]，新版 <faceType=…>。
            ("[<face,id=6/>]", ""),
            ("[<face,id=6>] 哈哈", "哈哈"),
            (r#"看看<faceType=6,faceId="0",ext="eA==">"#, "看看"),
            (r#"<faceType=6,faceId="0",ext="eA=="> 哈哈"#, "哈哈"),
            // 平台内部标记；internal 之后直到 `]` 都算标记。
            ("[internal: foo] 文字", "文字"),
            ("[Debug] x", "x"),
            ("[system]x", "x"),
            ("[internalism] x", "x"),
            // 合并转发摘要整条置空。
            ("[群聊的聊天记录] === 消息 1 === [消息内容] 早", ""),
            // 手打的符号与半截标记原样保留。
            ("无标记 < 保持", "无标记 < 保持"),
            ("a < b [c] d", "a < b [c] d"),
            ("<faceType=6 后面没了", "<faceType=6 后面没了"),
            ("<faceType=6,faceId=\"0\" x", "<faceType=6,faceId=\"0\" x"),
            ("说到[群聊的聊天记录]这个词", "说到[群聊的聊天记录]这个词"),
        ];
        for (content, expected) in cases {
            assert_eq!(sanitize_content(content), expected, "{content}");
        }
    }

    #[test]
    fn 纯表情消息没有正文段() {
        let payload = json!({ "content": r#"<faceType=6,faceId="0",ext="eA==">"# });
        assert!(message_segments(&payload).is_empty());
    }

    #[test]
    fn 引用信号与reply段原样透传() {
        // 各用例共用消息 id，各用一份入站去重表。
        let segments_of_quote = |content: &str, ext: Value, elements: Value| {
            let ctx = Context::for_tests();
            let payload = quote_event(content, ext, elements);
            segments_of(&translate(&ctx, "GROUP_MESSAGE_CREATE", &payload).unwrap())
        };

        let card = json!({
            "message_type": 103,
            "msg_idx": "REFIDX",
            "ark_data": { "ark_type": "miniapp", "fields": { "preview": "https://qq.ugcimg.cn/v1/PREVIEW" } },
            "content": "[卡片消息] 小程序",
        });
        // 元素 message_type=103 即引用，ext 没有 ref 也算；ext 带 ref_msg_idx 也算。
        let quoted = [
            (json!(["msg_idx=SELF"]), card),
            (json!(["ref_msg_idx=REFIDX"]), quoted_image()),
        ];
        for (ext, element) in quoted {
            let segments = segments_of_quote(" 666", ext, json!([element.clone()]));
            assert_eq!(segments[0].kind, "reply");
            // data 是被引元素原样：字段名与结构不变。
            assert_eq!(segments[0].data, element);
            // 卡片预览图、被引图片都不收进正文图片段，用户正文照常成段。
            assert_eq!(segments.len(), 2);
            assert_eq!(segments[1].data["text"], "666");
        }

        // ref_msg_idx 取值为空不算引用。
        let not_quoted = [
            json!(["ref_msg_idx=", "msg_idx=SELF"]),
            json!(["msg_idx=SELF"]),
        ];
        for ext in not_quoted {
            let plain = json!({ "message_type": 0, "content": "普通消息" });
            let segments = segments_of_quote(" 666", ext, json!([plain]));
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].kind, "text");
        }

        // 纯引用不打字只剩 reply 段。
        let ext = json!(["ref_msg_idx=REFIDX"]);
        let segments = segments_of_quote(" ", ext, json!([quoted_image()]));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].kind, "reply");
    }
}
