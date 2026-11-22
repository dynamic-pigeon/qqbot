//! 事件层 action → 官方 REST。
//!
//! 自有 action：`official_send`（scope + openid + 消息段，可带 reply_to
//! 被动回复）与 `official_member_info`（成员缓存查询）。
//! markdown 段按 msg_type=2 发送，平台拒绝时回退纯文本。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Context;
use crate::media::{MediaScope, MediaSource};
use crate::passive::{Conversation, conversation_key};

/// OneBot 风格错误码：1400 参数错，1404 能力不支持，1200 平台返回失败。
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
    let scope = match params["scope"].as_str() {
        Some("group") => {
            let openid = params["openid"]
                .as_str()
                .ok_or_else(|| ActionError::param("缺少 openid"))?;
            MediaScope::Group(openid.to_owned())
        }
        Some("private") => {
            let openid = params["openid"]
                .as_str()
                .ok_or_else(|| ActionError::param("缺少 openid"))?;
            MediaScope::User(openid.to_owned())
        }
        _ => return Err(ActionError::param("scope 必须是 group 或 private")),
    };
    let conversation = match &scope {
        MediaScope::Group(openid) => conversation_key(Conversation::Group, openid),
        MediaScope::User(openid) => conversation_key(Conversation::User, openid),
    };

    let plan = parse_segments(&params["message"])?;
    ctx.limiter.acquire(&conversation).await;

    let passive = pick_passive(&ctx.inbound, &conversation, params["reply_to"].as_str());

    let (sent_message_id, markdown_fallback) = match compose_markdown_plan(ctx, &plan).await {
        Some(composed) => {
            let mut standalone = composed.standalone;
            let (id, fallback) = match post_message(
                ctx,
                &scope,
                &markdown_body(&composed.markdown, passive.as_ref()),
            )
            .await
            {
                Ok(id) => (id, false),
                Err(markdown_error) => {
                    tracing::warn!("markdown 发送被拒，回退纯文本: {markdown_error}");
                    // 整条被拒时嵌入的图也没送达，一并转独立图片消息。
                    standalone.extend(
                        composed
                            .embeds
                            .iter()
                            .map(|image| MediaSource::Url(image.url.clone())),
                    );
                    // 相同 (msg_id, msg_seq) 会被平台判重，回退发送重新取槽位。
                    let fallback_passive =
                        pick_passive(&ctx.inbound, &conversation, params["reply_to"].as_str());
                    let id = post_message(
                        ctx,
                        &scope,
                        &send_body(&composed.text, None, fallback_passive.as_ref()),
                    )
                    .await
                    .map_err(|_| markdown_error)?;
                    (id, true)
                }
            };
            for source in standalone {
                let media = upload_media(ctx, &scope, source)
                    .await
                    .map_err(|e| ActionError::platform(e.to_string()))?;
                ctx.limiter.acquire(&conversation).await;
                let passive = ctx.inbound.take_reply_slot(&conversation);
                post_message(ctx, &scope, &send_body("", Some(&media), passive.as_ref())).await?;
            }
            (id, fallback)
        }
        // 纯图片消息：第一张带被动引用，其余逐张消耗槽位补发。
        None => {
            let mut images = plan.parts.into_iter().filter_map(|part| match part {
                PlanPart::Image(source) => Some(source),
                PlanPart::Text(_) => None,
            });
            let primary_media = match images.next() {
                Some(source) => Some(
                    upload_media(ctx, &scope, source)
                        .await
                        .map_err(|e| ActionError::platform(e.to_string()))?,
                ),
                None => None,
            };
            let sent = post_message(
                ctx,
                &scope,
                &send_body("", primary_media.as_deref(), passive.as_ref()),
            )
            .await?;
            for source in images {
                let media = upload_media(ctx, &scope, source)
                    .await
                    .map_err(|e| ActionError::platform(e.to_string()))?;
                ctx.limiter.acquire(&conversation).await;
                let passive = ctx.inbound.take_reply_slot(&conversation);
                post_message(ctx, &scope, &send_body("", Some(&media), passive.as_ref())).await?;
            }
            (sent, false)
        }
    };

    Ok(json!({
        "message_id": sent_message_id,
        "markdown_fallback": markdown_fallback,
    }))
}

/// 选被动回复槽位：显式 reply_to 优先消耗那条消息的余量，其次该会话最近
/// 一条入站消息。官方群推送（命令回执、定时任务）不带 msg_id 就是主动
/// 消息，需平台单独授权（实测默认无权限，HTTP 400 err_code 40034105），
/// 因此没有 reply_to 的发送也挂最近消息的槽位；槽位耗尽的裸发会被拒。
fn pick_passive(
    inbound: &crate::passive::RecentInbound,
    conversation: &str,
    reply_to: Option<&str>,
) -> Option<(String, u32)> {
    match reply_to {
        Some(id) => inbound
            .take_reply_slot_for(conversation, id)
            .or_else(|| inbound.take_reply_slot(conversation)),
        None => inbound.take_reply_slot(conversation),
    }
}

#[derive(Debug, PartialEq)]
enum PlanPart {
    Text(String),
    Image(MediaSource),
}

#[derive(Debug)]
struct SendPlan {
    parts: Vec<PlanPart>,
}

fn parse_segments(message: &Value) -> Result<SendPlan, ActionError> {
    let segments = message
        .as_array()
        .ok_or_else(|| ActionError::param("message 不是消息段数组"))?;
    let mut plan = SendPlan { parts: Vec::new() };

    for segment in segments {
        let kind = segment["type"].as_str().unwrap_or_default();
        match kind {
            "text" => {
                if let Some(text) = segment["data"]["text"].as_str() {
                    plan.parts.push(PlanPart::Text(text.to_owned()));
                }
            }
            "markdown" => {
                let content = segment["data"]["content"]
                    .as_str()
                    .ok_or_else(|| ActionError::param("markdown 段缺少 content"))?;
                plan.parts.push(PlanPart::Text(content.to_owned()));
            }
            "image" => {
                let file = segment["data"]["file"]
                    .as_str()
                    .ok_or_else(|| ActionError::param("image 段缺少 file"))?;
                plan.parts.push(PlanPart::Image(image_source(file)?));
            }
            "at" => {
                let openid = segment["data"]["qq"]
                    .as_str()
                    .ok_or_else(|| ActionError::param("at 段缺少 openid"))?;
                plan.parts.push(PlanPart::Text(format!(" <@{openid}>")));
            }
            // 引用由 reply_to 参数承载，事件层的 reply_and_quote 不产生该段；
            // 容忍外部构造的消息里出现。
            "reply" => {}
            other => return Err(ActionError::param(format!("不支持的消息段: {other}"))),
        }
    }
    Ok(plan)
}

/// 文本与图片按消息段原顺序合成一条 markdown：公网 URL 直接嵌，本地图先
/// 经素材接口拿 qqbot-media:// 引用；嵌入失败的图转独立图片消息补发。
/// 无文本段（纯图）返回 None，走 msg_type=7。
struct ComposedMarkdown {
    markdown: String,
    /// 回退纯文本用的正文（全部文本段拼接）。
    text: String,
    embeds: Vec<crate::media::MaterialRef>,
    standalone: Vec<MediaSource>,
}

async fn compose_markdown_plan(ctx: &Context, plan: &SendPlan) -> Option<ComposedMarkdown> {
    let has_text = plan
        .parts
        .iter()
        .any(|part| matches!(part, PlanPart::Text(text) if !text.trim().is_empty()));
    if !has_text {
        return None;
    }
    let mut pieces: Vec<MarkdownPiece> = Vec::new();
    let mut text = String::new();
    let mut embeds = Vec::new();
    let mut standalone = Vec::new();
    for part in &plan.parts {
        match part {
            PlanPart::Text(content) => {
                text.push_str(content);
                pieces.push(MarkdownPiece::Text(content.clone()));
            }
            PlanPart::Image(source) => match embed_reference(ctx, source).await {
                Ok(reference) => {
                    pieces.push(MarkdownPiece::Embed(reference.clone()));
                    embeds.push(reference);
                }
                Err(reason) => {
                    tracing::warn!("markdown 嵌图失败，改独立图片消息: {reason}");
                    standalone.push(source.clone());
                }
            },
        }
    }
    Some(ComposedMarkdown {
        markdown: compose_markdown(&pieces),
        text,
        embeds,
        standalone,
    })
}

enum MarkdownPiece {
    Text(String),
    Embed(crate::media::MaterialRef),
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

/// 解析一个图片段的 markdown 嵌入引用：URL 嗅探宽高，本地图走素材接口。
async fn embed_reference(
    ctx: &Context,
    source: &MediaSource,
) -> Result<crate::media::MaterialRef, String> {
    match source {
        MediaSource::Url(url) => remote_dimensions(ctx, url)
            .await
            .map(|(width, height)| crate::media::MaterialRef {
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
        Ok(MediaSource::Bytes {
            data,
            filename: "image.png".to_owned(),
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

fn markdown_body(content: &str, passive: Option<&(String, u32)>) -> Value {
    let mut body = json!({
        "msg_type": 2,
        "markdown": { "content": content },
    });
    if let Some((msg_id, msg_seq)) = passive {
        body["msg_id"] = json!(msg_id);
        body["msg_seq"] = json!(msg_seq);
    }
    body
}

fn send_body(content: &str, media: Option<&str>, passive: Option<&(String, u32)>) -> Value {
    let mut body = json!({});
    match media {
        Some(file_info) => {
            body["msg_type"] = json!(7);
            body["media"] = json!({ "file_info": file_info });
            // send.md：msg_type=7 时 content 需填值，空文本用一个空格。
            body["content"] = json!(if content.is_empty() { " " } else { content });
        }
        None => {
            body["msg_type"] = json!(0);
            body["content"] = json!(content);
        }
    }
    if let Some((msg_id, msg_seq)) = passive {
        body["msg_id"] = json!(msg_id);
        body["msg_seq"] = json!(msg_seq);
    }
    body
}

async fn upload_media(
    ctx: &Context,
    scope: &MediaScope,
    source: MediaSource,
) -> Result<String, crate::media::MediaError> {
    ctx.media
        .upload(&ctx.http, &ctx.token, ctx.config.api_base(), scope, source)
        .await
}

async fn post_message(
    ctx: &Context,
    scope: &MediaScope,
    body: &Value,
) -> Result<String, ActionError> {
    let authorization = ctx
        .token
        .authorization()
        .await
        .map_err(|e| ActionError::platform(e.to_string()))?;
    let response = ctx
        .http
        .post(format!("{}{}", ctx.config.api_base(), scope.send_path()))
        .header("Authorization", authorization)
        .json(body)
        .send()
        .await
        .map_err(|e| ActionError::platform(e.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ActionError::platform(e.to_string()))?;
    if !status.is_success() {
        return Err(ActionError::platform(format!("HTTP {status}: {text}")));
    }

    #[derive(Deserialize)]
    struct SendResponse {
        id: String,
    }
    let parsed: SendResponse = serde_json::from_str(&text)
        .map_err(|e| ActionError::platform(format!("发送响应解析失败（{text}）: {e}")))?;
    Ok(parsed.id)
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
    use super::*;
    use kovi::futures_util::FutureExt;

    #[test]
    fn 消息段解析保持顺序() {
        let message = json!([
            { "type": "text", "data": { "text": "标题" } },
            { "type": "image", "data": { "file": "https://cdn.example/wc.png" } },
            { "type": "markdown", "data": { "content": "**今日榜**" } },
        ]);
        let plan = parse_segments(&message).unwrap();
        let mut parts = plan.parts.into_iter();
        assert_eq!(parts.next().unwrap(), PlanPart::Text("标题".to_owned()));
        assert!(matches!(
            parts.next().unwrap(),
            PlanPart::Image(MediaSource::Url(_))
        ));
        assert_eq!(
            parts.next().unwrap(),
            PlanPart::Text("**今日榜**".to_owned())
        );
        assert!(parts.next().is_none());
    }

    #[test]
    fn at段拼接提及标记() {
        let message = json!([
            { "type": "text", "data": { "text": "恭喜" } },
            { "type": "at", "data": { "qq": "MEMBER-9" } },
        ]);
        let plan = parse_segments(&message).unwrap();
        let text: String = plan
            .parts
            .into_iter()
            .filter_map(|part| match part {
                PlanPart::Text(text) => Some(text),
                PlanPart::Image(_) => None,
            })
            .collect();
        assert_eq!(text, "恭喜 <@MEMBER-9>");
    }

    #[test]
    fn 未知消息段报参数错() {
        let message = json!([{ "type": "record", "data": { "file": "x.silk" } }]);
        let err = parse_segments(&message).unwrap_err();
        assert_eq!(err.retcode, 1400);
        assert!(err.message.contains("record"));
    }

    #[test]
    fn 图片uri三种形态() {
        assert!(matches!(
            image_source("https://a.example/x.png").unwrap(),
            MediaSource::Url(_)
        ));
        assert!(matches!(
            image_source("base64://aGVsbG8=").unwrap(),
            MediaSource::Bytes { .. }
        ));
        assert!(matches!(
            image_source("file:///tmp/a.png").unwrap(),
            MediaSource::FilePath(_)
        ));
        assert!(image_source("banana").is_err());
    }

    #[test]
    fn markdown合成按段序嵌图带尺寸() {
        use crate::media::MaterialRef;
        let composed = compose_markdown(&[
            MarkdownPiece::Text("标题".to_owned()),
            MarkdownPiece::Embed(MaterialRef {
                url: "qqbot-media://image/v1/TOKEN".to_owned(),
                width: 800,
                height: 400,
            }),
            MarkdownPiece::Text("数据行".to_owned()),
            MarkdownPiece::Embed(MaterialRef {
                url: "https://cdn.example/cover.jpg".to_owned(),
                width: 320,
                height: 208,
            }),
        ]);
        assert_eq!(
            composed,
            "标题\n\n![img #800px #400px](qqbot-media://image/v1/TOKEN)\n\n数据行\n\n![img #320px #208px](https://cdn.example/cover.jpg)"
        );
        // 图在段首时裁掉前导空行。
        let image_first = compose_markdown(&[
            MarkdownPiece::Embed(MaterialRef {
                url: "https://cdn.example/a.jpg".to_owned(),
                width: 100,
                height: 50,
            }),
            MarkdownPiece::Text("卡片信息".to_owned()),
        ]);
        assert_eq!(
            image_first,
            "![img #100px #50px](https://cdn.example/a.jpg)\n\n卡片信息"
        );
        assert_eq!(
            compose_markdown(&[MarkdownPiece::Text("纯文本".to_owned())]),
            "纯文本"
        );
    }

    #[test]
    fn 图片尺寸嗅探() {
        use crate::media::sniff_dimensions;
        // PNG 头：8 字节签名 + IHDR（宽 0x0100=256，高 0x00C8=200）。
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&256u32.to_be_bytes());
        png.extend_from_slice(&200u32.to_be_bytes());
        assert_eq!(sniff_dimensions(&png), Some((256, 200)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&208u16.to_le_bytes());
        assert_eq!(sniff_dimensions(&gif), Some((320, 208)));

        assert_eq!(sniff_dimensions(b"junk"), None);
    }

    #[test]
    fn 图片类型嗅探() {
        use crate::media::sniff_content_type;
        assert_eq!(
            sniff_content_type(&[0x89, b'P', b'N', b'G']),
            ("image/png", "png")
        );
        assert_eq!(
            sniff_content_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            ("image/jpeg", "jpg")
        );
        assert_eq!(sniff_content_type(b"GIF89a"), ("image/gif", "gif"));
        // 未知内容按 png 兜底，交给平台校验。
        assert_eq!(sniff_content_type(b"junk"), ("image/png", "png"));
    }

    #[test]
    fn 发送体组装() {
        let passive = ("MSG-1".to_owned(), 2u32);

        let markdown = markdown_body("**榜**", Some(&passive));
        assert_eq!(markdown["msg_type"], 2);
        assert_eq!(markdown["markdown"]["content"], "**榜**");
        assert_eq!(markdown["msg_id"], "MSG-1");

        let media = send_body("文本", Some("file-info-x"), Some(&passive));
        assert_eq!(media["msg_type"], 7);
        assert_eq!(media["media"]["file_info"], "file-info-x");
        assert_eq!(media["msg_seq"], 2);
        // send.md 要求 msg_type=7 填 content，纯图空文本落成一个空格。
        let pure_image = send_body("", Some("file-info-x"), None);
        assert_eq!(pure_image["content"], " ");

        let active = send_body("纯文本", None, None);
        assert_eq!(active["msg_type"], 0);
        assert!(active.get("msg_id").is_none());
    }

    #[test]
    fn 未支持action报错带名字() {
        let ctx = Context::for_tests();
        let err = handle_action(&ctx, "send_group_forward_msg", json!({}))
            .now_or_never()
            .unwrap()
            .unwrap_err();
        assert_eq!(err.retcode, 1404);
        assert!(err.message.contains("send_group_forward_msg"));
    }

    #[test]
    fn 无reply_to也挂最近入站槽位() {
        let inbound = crate::passive::RecentInbound::new();
        // 没有近期消息：无槽位，发送按主动消息处理。
        assert!(pick_passive(&inbound, "g:x", None).is_none());
        inbound.register("g:x", "msg-1");
        let (id, seq) = pick_passive(&inbound, "g:x", None).unwrap();
        assert_eq!((id.as_str(), seq), ("msg-1", 1));
    }

    #[test]
    fn 指定消息槽位耗尽回落到有余量的消息() {
        let inbound = crate::passive::RecentInbound::new();
        inbound.register("g:x", "msg-old");
        inbound.register("g:x", "msg-new");
        for seq in 1..=5 {
            let (id, got) = pick_passive(&inbound, "g:x", Some("msg-new")).unwrap();
            assert_eq!((id.as_str(), got), ("msg-new", seq));
        }
        let (id, seq) = pick_passive(&inbound, "g:x", Some("msg-new")).unwrap();
        assert_eq!((id.as_str(), seq), ("msg-old", 1));
    }

    #[test]
    fn 成员信息查缓存() {
        let ctx = Context::for_tests();
        ctx.members.update("G1", "U1", "昵称", None, None);
        let info = handle_action(
            &ctx,
            "official_member_info",
            json!({ "group_openid": "G1", "member_openid": "U1" }),
        )
        .now_or_never()
        .unwrap()
        .unwrap();
        assert_eq!(info["nickname"], "昵称");

        let miss = handle_action(
            &ctx,
            "official_member_info",
            json!({ "group_openid": "G?", "member_openid": "U?" }),
        )
        .now_or_never()
        .unwrap()
        .unwrap_err();
        assert_eq!(miss.retcode, 1200);
    }
}
