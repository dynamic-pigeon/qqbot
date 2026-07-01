use std::sync::LazyLock;
use std::time::Duration;

use kovi::serde_json;
use serde::Deserialize;

use crate::CLIENT;

/// 从 `BILIBILI_COOKIE` 环境变量读取 cookie 字符串。
/// 典型值: `"buvid3=xxx; b_nut=xxx"` 或 `"SESSDATA=xxx; buvid3=xxx"`。
/// 未设置或为空字符串时返回 None，请求保持无 Cookie。
///
/// 注意: cookie 包含 `SESSDATA` 等登录态凭据，绝对禁止在日志中打印明文。
static COOKIE: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("BILIBILI_COOKIE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
});

/// 默认 User-Agent，用于 `BILIBILI_USER_AGENT` 未设置时的 fallback。
/// 选用 Chrome Linux 最新稳定版的常见格式。
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Edg/147.0.0.0";

/// 从 `BILIBILI_USER_AGENT` 环境变量读取 UA 字符串；为空时回退到默认。
static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    std::env::var("BILIBILI_USER_AGENT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string())
});

pub const SPACE_FEED_URL: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space";

const DEFAULT_TIMEZONE_OFFSET: i32 = -480;
const DEFAULT_FEATURES: &str = "itemOpusStyle";

/// 预解析的硬编码 API URL。`Url::parse` 不是 const，所以放 LazyLock。
/// 若 `SPACE_FEED_URL` 被未来编辑损坏，这里会立刻以原文 panic，便于排查。
pub static SPACE_FEED_URL_PARSED: LazyLock<reqwest::Url> = LazyLock::new(|| {
    reqwest::Url::parse(SPACE_FEED_URL)
        .unwrap_or_else(|e| panic!("hardcoded SPACE_FEED_URL=`{SPACE_FEED_URL}` 解析失败: {e}"))
});

fn build_space_url(uid: u64, offset: Option<&str>) -> reqwest::Url {
    let mut url = SPACE_FEED_URL_PARSED.clone();
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("host_mid", &uid.to_string());
        q.append_pair("timezone_offset", &DEFAULT_TIMEZONE_OFFSET.to_string());
        q.append_pair("features", DEFAULT_FEATURES);
        if let Some(off) = offset {
            q.append_pair("offset", off);
        }
    }
    url
}

#[derive(thiserror::Error, Debug)]
pub enum DynamicsError {
    #[error("bilibili 动态 HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),
    #[error("bilibili 动态反序列化错误: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("bilibili 动态 API 错误 code={0} message={1}")]
    Api(i32, String),
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    code: i32,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<T>,
}

#[derive(Deserialize, Default)]
struct SpaceData {
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    items: Vec<serde_json::Value>,
    #[serde(default)]
    offset: String,
}

/// 拉取指定 UID 的 B 站用户空间动态。
///
/// 端点: <https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space>
///
/// **Env vars:** 通过 `.env` 文件或系统环境设置以下变量：
/// - `BILIBILI_COOKIE` — B 站 cookie（推荐 `buvid3=...; b_nut=...`），可降低被反爬拦截的概率
/// - `BILIBILI_USER_AGENT` — 自定义 UA 字符串；未设置时使用内置 Chrome 147 Linux
///
/// 注意: B 站 web dynamic 接口风控较严，依赖 host 的 User-Agent + Referer +
/// 其他浏览器 header 才可能返回 JSON。从被风控的 IP 调用会返回 HTML 验证码页
/// 或 `-412 request was banned` / `-352` 错误码。这些情况会落到
/// `DynamicsError::Http` (反序列化失败) 或 `DynamicsError::Api(code, msg)`。
pub async fn fetch_user_dynamics(
    uid: u64,
    offset: Option<&str>,
) -> Result<crate::dynamics::types::DynamicsPage, DynamicsError> {
    let url = build_space_url(uid, offset);
    let mut req = CLIENT.get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT.as_str())
        .header(reqwest::header::REFERER, "https://www.bilibili.com/");
    if let Some(c) = COOKIE.as_deref() {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = kovi::tokio::time::timeout(Duration::from_secs(10), req.send())
        .await
        .map_err(|_| DynamicsError::Api(-1, "请求超时".into()))??;

    let api: ApiResponse<SpaceData> = resp.json().await?;
    if api.code != 0 {
        return Err(DynamicsError::Api(api.code, api.message));
    }
    let data = api.data.unwrap_or_default();
    let next_offset = if data.offset.is_empty() {
        None
    } else {
        Some(data.offset)
    };
        Ok(DynamicsPage {
        items: data
            .items
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<ItemRaw>(v) {
                Ok(raw) => Some(convert_item(raw)),
                Err(e) => {
                    tracing::warn!("跳过无法反序列化的 dynamic item: {e}");
                    println!("跳过无法反序列化的 dynamic item: {e}");
                    None
                }
            })
            .collect(),
        has_more: data.has_more,
        next_offset,
    })
}

use crate::dynamics::types::{
    AuthorRaw, DescRaw, DynamicAuthor, DynamicBodyRaw, DynamicItem, DynamicsPage, ItemRaw, Pic,
    PicRaw, RichText,
};

fn convert_item(raw: ItemRaw) -> DynamicItem {
    let author = raw
        .modules
        .module_author
        .as_ref()
        .map(author_from_raw)
        .unwrap_or_default();
    let id = raw.id_str.clone();

    match raw.r#type.as_str() {
        "DYNAMIC_TYPE_AV" => convert_video(&id, &author, &raw.modules.module_dynamic)
            .unwrap_or(DynamicItem::Other { id: id.clone(), author: author.clone() }),
        "DYNAMIC_TYPE_DRAW" => {
            // 当前 B 站返回的图文动态用 MAJOR_TYPE_OPUS 承载；旧的 major.draw 已不再出现。
            convert_opus(&id, &author, &raw.modules.module_dynamic)
                .or_else(|| convert_draw(&id, &author, &raw.modules.module_dynamic))
                .unwrap_or(DynamicItem::Other { id: id.clone(), author: author.clone() })
        }
        "DYNAMIC_TYPE_WORD" => convert_opus(&id, &author, &raw.modules.module_dynamic)
            .or_else(|| convert_word(&id, &author, &raw.modules.module_dynamic))
            .unwrap_or(DynamicItem::Other { id: id.clone(), author: author.clone() }),
        "DYNAMIC_TYPE_ARTICLE" => convert_article(&id, &author, &raw.modules.module_dynamic)
            .unwrap_or(DynamicItem::Other { id: id.clone(), author: author.clone() }),
        "DYNAMIC_TYPE_LIVE" | "DYNAMIC_TYPE_LIVE_RCMD" => convert_live(&id, &author, &raw.modules.module_dynamic)
            .unwrap_or(DynamicItem::Other { id: id.clone(), author: author.clone() }),
        // 转发动态不再递归解析 orig，统一归为 Other，避免异常嵌套导致栈溢出或重复推送。
        "DYNAMIC_TYPE_FORWARD" => DynamicItem::Other { id, author },
        _ => DynamicItem::Other { id, author },
    }
}

fn author_from_raw(a: &AuthorRaw) -> DynamicAuthor {
    DynamicAuthor {
        name: a.name.clone(),
        pub_action: a.pub_action.clone(),
    }
}

fn summary_from_desc(desc: &DescRaw) -> Option<RichText> {
    let text = desc.text.clone();
    if text.is_empty() {
        None
    } else {
        Some(RichText { text })
    }
}

fn convert_opus(
    id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let major = body.major.as_ref()?;
    let opus = major.opus.as_ref()?;
    Some(DynamicItem::Opus {
        id: id.to_string(),
        title: opus.title.clone(),
        summary: opus.summary.as_ref().and_then(summary_from_desc),
        pics: opus.pics.iter().map(|p| p.url.clone()).collect(),
        jump_url: opus.jump_url.clone(),
        author: author.clone(),
    })
}

fn convert_video(
    id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let major = body.major.as_ref()?;
    let archive = major.archive.as_ref()?;
    Some(DynamicItem::Video {
        id: id.to_string(),
        bvid: archive.bvid.clone(),
        title: archive.title.clone(),
        cover_url: archive.cover.clone(),
        summary: body.desc.as_ref().and_then(summary_from_desc),
        author: author.clone(),
    })
}

fn convert_draw(
    id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let major = body.major.as_ref()?;
    let draw = major.draw.as_ref()?;
    Some(DynamicItem::Draw {
        id: id.to_string(),
        pics: draw.items.iter().map(pic_from_raw).collect(),
        summary: body.desc.as_ref().and_then(summary_from_desc),
        author: author.clone(),
    })
}

fn convert_word(
    id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let text = body
        .desc
        .as_ref()
        .map(|d| d.text.clone())
        .unwrap_or_default();
    let pics = body
        .major
        .as_ref()
        .and_then(|m| m.draw.as_ref())
        .map(|d| d.items.iter().map(pic_from_raw).collect::<Vec<_>>())
        .unwrap_or_default();
    Some(DynamicItem::Word {
        id: id.to_string(),
        text,
        pics,
        author: author.clone(),
    })
}

fn convert_article(
    _id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let major = body.major.as_ref()?;
    let art = major.article.as_ref()?;
    Some(DynamicItem::Article {
        id: art.id,
        title: art.title.clone(),
        summary: RichText {
            text: art.desc.clone(),
        },
        covers: art.covers.clone(),
        label: art.label.clone(),
        author: author.clone(),
    })
}

fn convert_live(
    _id: &str,
    author: &DynamicAuthor,
    body: &Option<DynamicBodyRaw>,
) -> Option<DynamicItem> {
    let body = body.as_ref()?;
    let major = body.major.as_ref()?;
    let live = major.live.as_ref()?;
    let room_id = room_id_from_jump_url(&live.jump_url);
    Some(DynamicItem::Live {
        id: live.id,
        title: live.title.clone(),
        cover_url: live.cover.clone(),
        room_id,
        author: author.clone(),
    })
}

/// 从 `live.jump_url` 解析 `room_id`。
/// 形如 `https://live.bilibili.com/12345` 或 `https://live.bilibili.com/12345?from=share` 或
/// `https://live.bilibili.com/12345/`。query / 尾斜杠 / 末尾空段都不能让解析失败。
fn room_id_from_jump_url(jump_url: &str) -> i64 {
    let Ok(parsed) = reqwest::Url::parse(jump_url) else {
        return 0;
    };
    parsed
        .path_segments()
        .map(|mut segs| {
            // 跳过末尾的空段（trailing slash），找到最后一个非空段
            segs.rfind(|s| !s.is_empty())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn pic_from_raw(p: &PicRaw) -> Pic {
    Pic { src: p.src.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    #[tokio::test]
    // 注意: 本测试在此开发 IP 上被 B 站风控拦截（HTML captcha / -412 / -352）。
    // 生产环境部署到干净 IP 时取消 ignore 即可正常联调。
    #[ignore = "bilibili web dynamic endpoint blocks this dev IP; pass when run from prod network"]
    async fn fetch_uid1_succeeds() {
        let page = fetch_user_dynamics(1, None)
            .await
            .expect("feed/space failed (likely anti-bot)");
        println!(
            "uid=1 has_more={} next_offset={:?} items={}",
            page.has_more,
            page.next_offset,
            page.items.len()
        );
        for item in page.items.iter().take(3) {
            println!("  {:?}", item);
        }
    }

    #[tokio::test]
    #[ignore = "bilibili web dynamic endpoint blocks this dev IP; pass when run from prod network"]
    async fn fetch_uid1_has_items() {
        let page = fetch_user_dynamics(1, None).await.unwrap();
        assert!(!page.items.is_empty(), "uid=1 应有动态");
    }

    #[tokio::test]
    #[ignore = "bilibili web dynamic endpoint blocks this dev IP; pass when run from prod network"]
    async fn fetch_uid1_paginate() {
        let first = fetch_user_dynamics(1, None).await.unwrap();
        if let Some(off) = first.next_offset.as_deref() {
            let second = fetch_user_dynamics(1, Some(off)).await.unwrap();
            println!("page2 items={}", second.items.len());
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn url_builds_without_offset() {
        let url = build_space_url(1, None);
        assert!(url.as_str().contains("host_mid=1"));
        assert!(url.as_str().contains("timezone_offset=-480"));
        assert!(url.as_str().contains("features=itemOpusStyle"));
    }

    #[test]
    fn url_builds_with_offset() {
        let url = build_space_url(2, Some("abc"));
        assert!(url.as_str().contains("host_mid=2"));
        assert!(url.as_str().contains("offset=abc"));
    }

    #[test]
    fn room_id_parses_basic_url() {
        assert_eq!(room_id_from_jump_url("https://live.bilibili.com/12345"), 12345);
    }

    #[test]
    fn room_id_parses_url_with_query_string() {
        assert_eq!(
            room_id_from_jump_url("https://live.bilibili.com/12345?from=share&spm_id_from=.."),
            12345
        );
    }

    #[test]
    fn room_id_parses_url_with_trailing_slash() {
        assert_eq!(room_id_from_jump_url("https://live.bilibili.com/12345/"), 12345);
    }

    #[test]
    fn room_id_returns_zero_for_garbage() {
        assert_eq!(room_id_from_jump_url("not a url"), 0);
        assert_eq!(room_id_from_jump_url(""), 0);
        assert_eq!(room_id_from_jump_url("https://live.bilibili.com/abc"), 0);
        // 纯前缀不能解析出 id
        assert_eq!(room_id_from_jump_url("//"), 0);
    }
}
