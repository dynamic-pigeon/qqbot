//! 事件层 action → 官方 REST。
//!
//! 自有 action：`official_send`（scope + openid + 消息段，可带 reply_to
//! 被动回复）与 `official_member_info`（成员缓存查询）。
//! 有文本的消息按 msg_type=2（markdown）发送，平台拒绝时回退纯文本。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Context;
use crate::media::{MaterialRef, MediaError, MediaSource, sniff_content_type};
use crate::passive::{RecentInbound, ReplySlot};
use crate::token::TokenError;
use crate::{ChatScope, PostError, REST_TIMEOUT, post_json};

/// 错误码沿用 OneBot 惯例：1400 参数错，1404 能力不支持，1200 平台返回失败。
#[derive(Debug)]
pub struct ActionError {
    pub retcode: i32,
    pub message: String,
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.retcode, self.message)
    }
}

impl ActionError {
    fn param(message: impl Into<String>) -> Self {
        Self {
            retcode: 1400,
            message: message.into(),
        }
    }

    fn platform(message: impl Into<String>) -> Self {
        Self {
            retcode: 1200,
            message: message.into(),
        }
    }

    fn unsupported(action: &str) -> Self {
        Self {
            retcode: 1404,
            message: format!("官方 API 不支持 action: {action}"),
        }
    }
}

impl From<TokenError> for ActionError {
    fn from(error: TokenError) -> Self {
        Self::platform(error.to_string())
    }
}

impl From<PostError> for ActionError {
    fn from(error: PostError) -> Self {
        Self::platform(error.to_string())
    }
}

impl From<MediaError> for ActionError {
    fn from(error: MediaError) -> Self {
        Self::platform(error.to_string())
    }
}

pub async fn handle_action(
    ctx: &Context,
    action: &str,
    params: Value,
) -> Result<Value, ActionError> {
    match action {
        "official_send" => official_send(ctx, &params).await,
        "official_member_info" => member_info(ctx, &params),
        _ => Err(ActionError::unsupported(action)),
    }
}

async fn official_send(ctx: &Context, params: &Value) -> Result<Value, ActionError> {
    let scope = parse_scope(params)?;
    let parts = parse_segments(&params["message"])?;
    let reply_to = params["reply_to"].as_str();

    let sent_message_id = match compose_markdown_plan(ctx, &parts).await {
        Some(composed) => send_markdown(ctx, &scope, &parts, reply_to, composed).await?,
        None => send_images(ctx, &scope, reply_to, images_of(&parts))
            .await?
            .ok_or_else(|| ActionError::param("消息没有可发送的内容"))?,
    };
    Ok(json!({ "message_id": sent_message_id }))
}

/// action 参数里的 scope + openid。
fn parse_scope(params: &Value) -> Result<ChatScope, ActionError> {
    let is_group = match params["scope"].as_str() {
        Some("group") => true,
        Some("private") => false,
        _ => return Err(ActionError::param("scope 必须是 group 或 private")),
    };
    let openid = params["openid"]
        .as_str()
        .ok_or_else(|| ActionError::param("缺少 openid"))?;
    Ok(if is_group {
        ChatScope::Group(openid.to_owned())
    } else {
        ChatScope::User(openid.to_owned())
    })
}

/// 发送合成的 markdown。整条被平台拒绝时回退纯文本，图片全部改独立图片消息。
async fn send_markdown(
    ctx: &Context,
    scope: &ChatScope,
    parts: &[PlanPart],
    reply_to: Option<&str>,
    composed: ComposedMarkdown,
) -> Result<String, ActionError> {
    ctx.limiter.acquire(&scope.key()).await;
    let passive = pick_passive(&ctx.inbound, scope, reply_to);
    let sent = match post_message(
        ctx,
        scope,
        &markdown_body(&composed.markdown, passive.as_ref()),
    )
    .await
    {
        Ok(sent) => {
            send_images(ctx, scope, None, composed.standalone.into_iter()).await?;
            sent
        }
        Err(markdown_error) => {
            tracing::warn!("markdown 发送被拒，回退纯文本: {markdown_error}");
            // 相同 (msg_id, msg_seq) 会被平台判重，回退发送重新取槽位。
            ctx.limiter.acquire(&scope.key()).await;
            let passive = pick_passive(&ctx.inbound, scope, reply_to);
            let sent = post_message(ctx, scope, &text_body(&composed.text, passive.as_ref()))
                .await
                .inspect_err(|error| tracing::warn!("纯文本回退发送失败: {error}"))?;
            send_images(ctx, scope, None, images_of(parts)).await?;
            sent
        }
    };
    Ok(sent)
}

/// 逐张发送图片，每张一条 msg_type=7 消息，返回首条的消息 id；没有图片
/// 返回 None。
async fn send_images(
    ctx: &Context,
    scope: &ChatScope,
    reply_to: Option<&str>,
    mut images: impl Iterator<Item = MediaSource>,
) -> Result<Option<String>, ActionError> {
    let Some(first) = images.next() else {
        return Ok(None);
    };

    let media = upload_image(ctx, scope, first).await?;
    ctx.limiter.acquire(&scope.key()).await;
    let passive = pick_passive(&ctx.inbound, scope, reply_to);
    let sent = post_message(ctx, scope, &image_body(&media, passive.as_ref())).await?;

    for image in images {
        let media = upload_image(ctx, scope, image).await?;
        ctx.limiter.acquire(&scope.key()).await;
        let passive = ctx.inbound.take_reply_slot(scope);
        post_message(ctx, scope, &image_body(&media, passive.as_ref())).await?;
    }
    Ok(Some(sent))
}

/// 选被动回复槽位：显式 reply_to 优先消耗那条消息的余量，其次该会话最近一条
/// 入站消息。不带 msg_id 的发送是主动消息，需平台单独授权（群聊默认无权限），
/// 所以没有 reply_to 的发送同样挂最近消息的槽位；槽位耗尽的裸发会被拒。
fn pick_passive(
    inbound: &RecentInbound,
    scope: &ChatScope,
    reply_to: Option<&str>,
) -> Option<ReplySlot> {
    match reply_to {
        Some(id) => inbound
            .take_reply_slot_for(scope, id)
            .or_else(|| inbound.take_reply_slot(scope)),
        None => inbound.take_reply_slot(scope),
    }
}

#[derive(Debug, PartialEq)]
enum PlanPart {
    Text(String),
    Image(MediaSource),
}

fn parse_segments(message: &Value) -> Result<Vec<PlanPart>, ActionError> {
    let segments = message
        .as_array()
        .ok_or_else(|| ActionError::param("message 不是消息段数组"))?;

    let mut parts = Vec::new();
    for segment in segments {
        let data = &segment["data"];
        let part = match segment["type"].as_str().unwrap_or_default() {
            "text" => match data["text"].as_str() {
                Some(text) => PlanPart::Text(text.to_owned()),
                None => continue,
            },
            "markdown" => PlanPart::Text(segment_text(data, "content")?.to_owned()),
            "image" => PlanPart::Image(image_source(segment_text(data, "file")?)?),
            "at" => PlanPart::Text(format!(" <@{}>", segment_text(data, "qq")?)),
            // 引用由 reply_to 参数承载；容忍外部构造的消息里出现。
            "reply" => continue,
            other => return Err(ActionError::param(format!("不支持的消息段: {other}"))),
        };
        parts.push(part);
    }
    Ok(parts)
}

/// 消息段 data 里的必填文本字段。
fn segment_text<'a>(data: &'a Value, key: &str) -> Result<&'a str, ActionError> {
    data[key]
        .as_str()
        .ok_or_else(|| ActionError::param(format!("消息段缺少 {key}")))
}

fn images_of(parts: &[PlanPart]) -> impl Iterator<Item = MediaSource> + '_ {
    parts.iter().filter_map(|part| match part {
        PlanPart::Image(source) => Some(source.clone()),
        PlanPart::Text(_) => None,
    })
}

/// 文本与图片按消息段原顺序合成的一条 markdown。
struct ComposedMarkdown {
    markdown: String,
    /// 回退纯文本用的正文（全部文本段拼接）。
    text: String,
    /// 嵌入失败、改独立图片消息补发的图。
    standalone: Vec<MediaSource>,
}

/// 有文本段才合成 markdown；纯图片返回 None，走 msg_type=7。
async fn compose_markdown_plan(ctx: &Context, parts: &[PlanPart]) -> Option<ComposedMarkdown> {
    let has_text = parts
        .iter()
        .any(|part| matches!(part, PlanPart::Text(text) if !text.trim().is_empty()));
    if !has_text {
        return None;
    }

    let mut pieces = Vec::new();
    let mut standalone = Vec::new();
    for part in parts {
        match part {
            PlanPart::Text(text) => pieces.push(MarkdownPiece::Text(text)),
            PlanPart::Image(source) => match embed_reference(ctx, source).await {
                Ok(image) => pieces.push(MarkdownPiece::Embed(image)),
                Err(reason) => {
                    tracing::warn!("markdown 嵌图失败，改独立图片消息: {reason}");
                    standalone.push(source.clone());
                }
            },
        }
    }

    let text = pieces
        .iter()
        .filter_map(|piece| match piece {
            MarkdownPiece::Text(text) => Some(*text),
            _ => None,
        })
        .collect();
    Some(ComposedMarkdown {
        markdown: compose_markdown(&pieces),
        text,
        standalone,
    })
}

enum MarkdownPiece<'a> {
    Text(&'a str),
    Embed(MaterialRef),
}

/// 图片语法必须带像素宽高，否则平台渲染成 `[img]` 纯文本。
fn compose_markdown(pieces: &[MarkdownPiece]) -> String {
    let mut markdown = String::new();
    for piece in pieces {
        match piece {
            MarkdownPiece::Text(text) => markdown.push_str(text),
            MarkdownPiece::Embed(image) => {
                markdown.push_str(&format!(
                    "\n\n![img #{}px #{}px]({})\n\n",
                    image.width, image.height, image.url
                ));
            }
        }
    }
    markdown.trim().to_owned()
}

/// 一个图片段的 markdown 嵌入引用：URL 嗅探宽高，本地图走素材接口。
async fn embed_reference(ctx: &Context, source: &MediaSource) -> Result<MaterialRef, String> {
    match source {
        MediaSource::Url(url) => remote_dimensions(ctx, url)
            .await
            .map(|(width, height)| MaterialRef {
                url: url.clone(),
                width,
                height,
            })
            .ok_or_else(|| format!("远程图尺寸嗅探失败: {url}")),
        local => crate::media::material_url(
            &ctx.http,
            &ctx.token,
            ctx.config.api_base(),
            &ctx.media,
            local,
        )
        .await
        .map_err(|e| e.to_string()),
    }
}

fn image_source(file: &str) -> Result<MediaSource, ActionError> {
    if let Some(encoded) = file.strip_prefix("base64://") {
        let data = BASE64
            .decode(encoded)
            .map_err(|e| ActionError::param(format!("base64 图片解码失败: {e}")))?;
        let extension = sniff_content_type(&data).1;
        Ok(MediaSource::Bytes {
            data,
            filename: format!("image.{extension}"),
        })
    } else if let Some(path) = file.strip_prefix("file://") {
        Ok(MediaSource::FilePath(path.to_owned()))
    } else if file.starts_with("http://") || file.starts_with("https://") {
        Ok(MediaSource::Url(file.to_owned()))
    } else {
        Err(ActionError::param(format!("不支持的图片 URI: {file}")))
    }
}

/// 下载远程图并嗅探尺寸；markdown 嵌图需要宽高，取不到就转独立图片消息。
async fn remote_dimensions(ctx: &Context, url: &str) -> Option<(u32, u32)> {
    let bytes = ctx
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;
    crate::media::sniff_dimensions(&bytes)
}

/// 被动回复带上目标消息与序号。
fn with_passive(mut body: Value, passive: Option<&ReplySlot>) -> Value {
    if let Some((msg_id, msg_seq)) = passive {
        body["msg_id"] = json!(msg_id);
        body["msg_seq"] = json!(msg_seq);
    }
    body
}

fn markdown_body(content: &str, passive: Option<&ReplySlot>) -> Value {
    with_passive(
        json!({
            "msg_type": 2,
            "markdown": { "content": content },
        }),
        passive,
    )
}

fn text_body(content: &str, passive: Option<&ReplySlot>) -> Value {
    with_passive(
        json!({
            "msg_type": 0,
            "content": content,
        }),
        passive,
    )
}

/// 纯图消息不带 content（qqbot-nodejs sendMediaMessage 同款）。
fn image_body(file_info: &str, passive: Option<&ReplySlot>) -> Value {
    with_passive(
        json!({
            "msg_type": 7,
            "media": { "file_info": file_info },
        }),
        passive,
    )
}

async fn upload_image(
    ctx: &Context,
    scope: &ChatScope,
    source: MediaSource,
) -> Result<String, ActionError> {
    ctx.media
        .upload(&ctx.http, &ctx.token, ctx.config.api_base(), scope, source)
        .await
        .map_err(ActionError::from)
}

#[derive(Deserialize)]
struct SendResponse {
    id: String,
}

async fn post_message(
    ctx: &Context,
    scope: &ChatScope,
    body: &Value,
) -> Result<String, ActionError> {
    let authorization = ctx.token.authorization().await?;
    let text = post_json(
        &ctx.http,
        &authorization,
        format!("{}{}/messages", ctx.config.api_base(), scope.base_path()),
        body,
        REST_TIMEOUT,
    )
    .await?;
    let response: SendResponse = serde_json::from_str(&text)
        .map_err(|e| ActionError::platform(format!("发送响应解析失败（{text}）: {e}")))?;
    Ok(response.id)
}

fn member_info(ctx: &Context, params: &Value) -> Result<Value, ActionError> {
    let group_openid = params["group_openid"]
        .as_str()
        .ok_or_else(|| ActionError::param("缺少 group_openid"))?;
    let member_openid = params["member_openid"]
        .as_str()
        .ok_or_else(|| ActionError::param("缺少 member_openid"))?;
    ctx.members
        .member_info(group_openid, member_openid)
        .ok_or_else(|| {
            ActionError::platform("该成员近期未在群内发言，官方 API 无法主动查询成员信息")
        })
}

#[cfg(test)]
mod tests {
    use kovi::futures_util::FutureExt;

    use super::*;
    use crate::media::MaterialRef;

    #[test]
    fn 消息段按原顺序解析() {
        let message = json!([
            { "type": "at", "data": { "qq": "MEMBER-9" } },
            { "type": "text", "data": { "text": "标题" } },
            { "type": "reply", "data": { "id": "1" } },
            { "type": "image", "data": { "file": "https://cdn.example/wc.png" } },
            { "type": "markdown", "data": { "content": "**今日榜**" } },
        ]);
        let parts = parse_segments(&message).unwrap();
        assert_eq!(parts[0], PlanPart::Text(" <@MEMBER-9>".to_owned()));
        assert_eq!(parts[1], PlanPart::Text("标题".to_owned()));
        assert!(matches!(parts[2], PlanPart::Image(MediaSource::Url(_))));
        assert_eq!(parts[3], PlanPart::Text("**今日榜**".to_owned()));
        assert_eq!(parts.len(), 4);
    }

    #[test]
    fn 未知消息段或缺字段报参数错() {
        let unknown = json!([{ "type": "record", "data": { "file": "x.silk" } }]);
        assert_eq!(parse_segments(&unknown).unwrap_err().retcode, 1400);

        let missing = json!([{ "type": "image", "data": {} }]);
        let err = parse_segments(&missing).unwrap_err();
        assert!(err.message.contains("file"));
    }

    #[test]
    fn 图片来源与base64扩展名() {
        assert!(matches!(
            image_source("https://a.example/x.png").unwrap(),
            MediaSource::Url(_)
        ));
        assert!(matches!(
            image_source("file:///tmp/a.png").unwrap(),
            MediaSource::FilePath(_)
        ));
        assert!(image_source("banana").is_err());

        let filename = |file: &str| match image_source(file).unwrap() {
            MediaSource::Bytes { filename, .. } => filename,
            _ => unreachable!("base64 图片是字节来源"),
        };
        assert_eq!(filename("base64://iVBORw0KGgo="), "image.png");
        assert_eq!(filename("base64:///9j/"), "image.jpg");
        assert_eq!(filename("base64://R0lGODlh"), "image.gif");
    }

    #[test]
    fn markdown合成按段序嵌图带尺寸() {
        let embed = |url: &str, width, height| {
            MarkdownPiece::Embed(MaterialRef {
                url: url.to_owned(),
                width,
                height,
            })
        };
        let composed = compose_markdown(&[
            MarkdownPiece::Text("标题"),
            embed("qqbot-media://image/v1/TOKEN", 800, 400),
            MarkdownPiece::Text("数据行"),
            embed("https://cdn.example/cover.jpg", 320, 208),
        ]);
        assert_eq!(
            composed,
            "标题\n\n![img #800px #400px](qqbot-media://image/v1/TOKEN)\n\n数据行\n\n![img #320px #208px](https://cdn.example/cover.jpg)"
        );
        // 图在段首时裁掉前导空行。
        assert_eq!(
            compose_markdown(&[
                embed("https://cdn.example/a.jpg", 100, 50),
                MarkdownPiece::Text("卡片信息"),
            ]),
            "![img #100px #50px](https://cdn.example/a.jpg)\n\n卡片信息"
        );
    }

    #[test]
    fn 发送体形状() {
        let passive = ("MSG-1".to_owned(), 2u32);
        assert_eq!(
            markdown_body("**榜**", Some(&passive)),
            json!({ "msg_type": 2, "markdown": { "content": "**榜**" }, "msg_id": "MSG-1", "msg_seq": 2 })
        );
        // 纯图消息不带 content 字段。
        assert_eq!(
            image_body("file-info-x", Some(&passive)),
            json!({ "msg_type": 7, "media": { "file_info": "file-info-x" }, "msg_id": "MSG-1", "msg_seq": 2 })
        );
        assert_eq!(
            text_body("纯文本", None),
            json!({ "msg_type": 0, "content": "纯文本" })
        );
    }

    #[test]
    fn action分发() {
        let ctx = Context::for_tests();
        let run = |action: &str, params: Value| {
            handle_action(&ctx, action, params).now_or_never().unwrap()
        };

        let unsupported = run("send_group_forward_msg", json!({})).unwrap_err();
        assert_eq!(unsupported.retcode, 1404);

        let member_info = |group: &str, member: &str| {
            run(
                "official_member_info",
                json!({ "group_openid": group, "member_openid": member }),
            )
        };
        ctx.members.update("G1", "U1", "昵称", None, None);
        assert_eq!(member_info("G1", "U1").unwrap()["nickname"], "昵称");
        assert_eq!(member_info("G?", "U?").unwrap_err().retcode, 1200);
    }

    #[test]
    fn 被动槽位的选择() {
        let inbound = RecentInbound::new();
        let scope = ChatScope::Group("x".to_owned());
        let slot = |id: &str, seq: u32| Some((id.to_owned(), seq));
        assert_eq!(pick_passive(&inbound, &scope, None), None);

        inbound.register(&scope, "msg-old");
        inbound.register(&scope, "msg-new");
        // 没指定消息时取该会话最新的一条。
        assert_eq!(pick_passive(&inbound, &scope, None), slot("msg-new", 1));
        for seq in 2..=5 {
            let chosen = pick_passive(&inbound, &scope, Some("msg-new"));
            assert_eq!(chosen, slot("msg-new", seq));
        }
        // 指定的消息用满后回落到还有余量的消息。
        let chosen = pick_passive(&inbound, &scope, Some("msg-new"));
        assert_eq!(chosen, slot("msg-old", 1));
    }
}
