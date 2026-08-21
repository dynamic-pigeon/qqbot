use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use kovi::serde_json;
use md5::{Digest as _, Md5};
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE as COOKIE_HEADER};
use serde::Deserialize;

use crate::CLIENT;

/// 从 `BILIBILI_COOKIE` 环境变量读取可选的 cookie 字符串。
/// 典型值: `"buvid3=xxx; b_nut=xxx"` 或 `"SESSDATA=xxx; buvid3=xxx"`。
/// 未设置或为空字符串时自动获取 Bilibili 游客 Cookie。
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

/// 从 `BILIBILI_USER_AGENT` 环境变量解析 UA；未设置或空白时回退到默认。
fn resolve_user_agent(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string())
}

/// HTTP 直连与 Chromium 后备共用同一份 UA，避免两条路径指纹不一致。
static USER_AGENT: LazyLock<String> =
    LazyLock::new(|| resolve_user_agent(std::env::var("BILIBILI_USER_AGENT").ok().as_deref()));

pub(super) fn user_agent() -> &'static str {
    USER_AGENT.as_str()
}

/// 直连 API 响应字节上限：正常动态 JSON 只有几百 KB，超限说明响应异常
/// （被风控返回 HTML 或超大 payload），与 Chromium 后备路径的上限一致。
const MAX_API_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub const SPACE_FEED_URL: &str = "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space";
const FINGER_SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";
const NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";

const DEFAULT_TIMEZONE_OFFSET: i32 = -480;
const DEFAULT_FEATURES: &str = "itemOpusStyle,listOnlyfans,opusBigCover,onlyfansVote,\
forwardListHidden,decorationCard,commentsNewVersion,onlyfansAssetsV2,ugcDelete,onlyfansQaCard,\
avatarAutoTheme,sunflowerStyle,cardsEnhance,eva3CardOpus,eva3CardVideo,eva3CardComment,eva3CardUser";
const WEB_LOCATION: &str = "333.1387";
const SESSION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[derive(Clone)]
struct WebSession {
    cookie: String,
    mixin_key: String,
    expires_at: Instant,
}

static WEB_SESSION: kovi::tokio::sync::Mutex<Option<WebSession>> =
    kovi::tokio::sync::Mutex::const_new(None);

/// 预解析的硬编码 API URL。`Url::parse` 不是 const，所以放 LazyLock。
/// 若 `SPACE_FEED_URL` 被未来编辑损坏，这里会立刻以原文 panic，便于排查。
pub static SPACE_FEED_URL_PARSED: LazyLock<reqwest::Url> = LazyLock::new(|| {
    reqwest::Url::parse(SPACE_FEED_URL)
        .unwrap_or_else(|e| panic!("hardcoded SPACE_FEED_URL=`{SPACE_FEED_URL}` 解析失败: {e}"))
});

fn build_space_params(uid: u64, offset: Option<&str>) -> BTreeMap<String, String> {
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let viewport_seed = now_millis ^ uid;
    let dm_img_inter = serde_json::json!({
        "ds": [],
        "wh": [
            4000 + viewport_seed % 311,
            4300 + (viewport_seed / 7) % 311,
            1 + (viewport_seed / 13) % 100
        ],
        "of": [
            100 + (viewport_seed / 17) % 301,
            200 + (viewport_seed / 19) % 601,
            100 + (viewport_seed / 17) % 301
        ]
    });

    BTreeMap::from([
        ("offset".into(), offset.unwrap_or_default().into()),
        ("host_mid".into(), uid.to_string()),
        (
            "timezone_offset".into(),
            DEFAULT_TIMEZONE_OFFSET.to_string(),
        ),
        ("platform".into(), "web".into()),
        ("features".into(), DEFAULT_FEATURES.into()),
        ("web_location".into(), WEB_LOCATION.into()),
        ("dm_img_list".into(), "[]".into()),
        (
            "dm_img_str".into(),
            URL_SAFE_NO_PAD.encode("WebGL 1.0 (OpenGL ES 2.0 Chromium)"),
        ),
        (
            "dm_cover_img_str".into(),
            URL_SAFE_NO_PAD.encode(
                "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0x0000C0DE)), \
                 SwiftShader driver)",
            ),
        ),
        ("dm_img_inter".into(), dm_img_inter.to_string()),
        (
            "x-bili-device-req-json".into(),
            serde_json::json!({
                "platform": "web",
                "device": "pc",
                "spmid": WEB_LOCATION
            })
            .to_string(),
        ),
    ])
}

fn signed_query(mut params: BTreeMap<String, String>, mixin_key: &str, wts: u64) -> String {
    params.insert("wts".into(), wts.to_string());
    let query = params
        .iter()
        .map(|(key, value)| {
            let filtered: String = value
                .chars()
                .filter(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*'))
                .collect();
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(&filtered)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let digest = Md5::digest(format!("{query}{mixin_key}").as_bytes());
    let mut w_rid = String::with_capacity(32);
    for byte in digest {
        write!(&mut w_rid, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("{query}&w_rid={w_rid}")
}

fn build_space_url(uid: u64, offset: Option<&str>, mixin_key: &str, wts: u64) -> reqwest::Url {
    let mut url = SPACE_FEED_URL_PARSED.clone();
    url.set_query(Some(&signed_query(
        build_space_params(uid, offset),
        mixin_key,
        wts,
    )));
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
    #[error("bilibili 动态响应异常 HTTP {status}, content-type={content_type}")]
    UnexpectedResponse { status: u16, content_type: String },
    #[error("bilibili 匿名会话初始化失败: {0}")]
    Session(String),
    #[error("bilibili Chromium 后备请求失败: {0}")]
    Browser(#[source] anyhow::Error),
    #[error("bilibili 动态响应超过大小上限: {0}")]
    BodyLimit(#[source] anyhow::Error),
}

impl DynamicsError {
    fn is_risk_control(&self) -> bool {
        matches!(
            self,
            Self::Api(-101 | -352 | -412, _) | Self::UnexpectedResponse { .. } | Self::BodyLimit(_)
        )
    }
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
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

#[derive(Deserialize)]
struct NavData {
    wbi_img: WbiImg,
}

#[derive(Deserialize)]
struct WbiImg {
    img_url: String,
    sub_url: String,
}

#[derive(Deserialize)]
struct FingerData {
    b_3: String,
}

fn guest_cookie(buvid3: &str, timestamp: u64) -> Result<String, DynamicsError> {
    if buvid3.is_empty()
        || !buvid3
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(DynamicsError::Session(
            "finger/spi 返回的 buvid3 无效".into(),
        ));
    }
    Ok(format!("buvid3={buvid3}; b_nut={timestamp}"))
}

fn wbi_resource_key(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let file = parsed.path_segments()?.next_back()?;
    Some(
        file.split_once('.')
            .map_or(file, |(stem, _)| stem)
            .to_string(),
    )
}

fn mixin_key(img_url: &str, sub_url: &str) -> Result<String, DynamicsError> {
    let source = format!(
        "{}{}",
        wbi_resource_key(img_url)
            .ok_or_else(|| DynamicsError::Session("WBI img_url 无效".into()))?,
        wbi_resource_key(sub_url)
            .ok_or_else(|| DynamicsError::Session("WBI sub_url 无效".into()))?
    );
    let bytes = source.as_bytes();
    if MIXIN_KEY_ENC_TAB.iter().any(|&index| index >= bytes.len()) {
        return Err(DynamicsError::Session("WBI key 长度异常".into()));
    }
    Ok(MIXIN_KEY_ENC_TAB
        .iter()
        .take(32)
        .map(|&index| bytes[index] as char)
        .collect())
}

async fn bootstrap_guest_cookie() -> Result<String, DynamicsError> {
    let response = CLIENT
        .get(FINGER_SPI_URL)
        .header(ACCEPT, "application/json, text/plain, */*")
        .send()
        .await?;
    let api: ApiResponse<FingerData> = response.json().await?;
    if api.code != 0 {
        return Err(DynamicsError::Api(api.code, api.message));
    }
    let fingerprint = api
        .data
        .ok_or_else(|| DynamicsError::Session("finger/spi 响应缺少游客标识".into()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    guest_cookie(&fingerprint.b_3, timestamp)
}

async fn fetch_mixin_key(cookie: &str) -> Result<String, DynamicsError> {
    let response = CLIENT
        .get(NAV_URL)
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(COOKIE_HEADER, cookie)
        .send()
        .await?;
    let api: ApiResponse<NavData> = response.json().await?;
    let images = match api.data {
        Some(data) => data.wbi_img,
        None if api.code != 0 => return Err(DynamicsError::Api(api.code, api.message)),
        None => return Err(DynamicsError::Session("nav 响应缺少 WBI 图片信息".into())),
    };
    mixin_key(&images.img_url, &images.sub_url)
}

async fn web_session(force_refresh: bool) -> Result<WebSession, DynamicsError> {
    let mut state = WEB_SESSION.lock().await;
    if !force_refresh
        && let Some(session) = state.as_ref()
        && session.expires_at > Instant::now()
    {
        return Ok(session.clone());
    }

    let cookie = match COOKIE.as_ref() {
        Some(cookie) => cookie.clone(),
        None => bootstrap_guest_cookie().await?,
    };
    let session = WebSession {
        mixin_key: fetch_mixin_key(&cookie).await?,
        cookie,
        expires_at: Instant::now() + SESSION_TTL,
    };
    *state = Some(session.clone());
    Ok(session)
}

async fn fetch_page(
    uid: u64,
    offset: Option<&str>,
    session: &WebSession,
) -> Result<DynamicsPage, DynamicsError> {
    let wts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let url = build_space_url(uid, offset, &session.mixin_key, wts);
    let response = CLIENT
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT.as_str())
        .header(
            reqwest::header::REFERER,
            format!("https://space.bilibili.com/{uid}/dynamic"),
        )
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(COOKIE_HEADER, &session.cookie)
        .send()
        .await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !status.is_success() {
        return Err(DynamicsError::UnexpectedResponse {
            status: status.as_u16(),
            content_type,
        });
    }
    let body = utils::read_response_limited(response, MAX_API_RESPONSE_BYTES)
        .await
        .map_err(DynamicsError::BodyLimit)?;
    let api: ApiResponse<SpaceData> = serde_json::from_slice(&body).map_err(|error| {
        if content_type.contains("json") {
            DynamicsError::Deserialize(error)
        } else {
            DynamicsError::UnexpectedResponse {
                status: status.as_u16(),
                content_type,
            }
        }
    })?;
    if api.code != 0 {
        return Err(DynamicsError::Api(api.code, api.message));
    }
    Ok(convert_page_data(api.data.unwrap_or_default()))
}

async fn fetch_page_with_browser(
    uid: u64,
    offset: Option<&str>,
) -> Result<DynamicsPage, DynamicsError> {
    let body = super::browser::fetch_space_body(uid, offset)
        .await
        .map_err(DynamicsError::Browser)?;
    let api: ApiResponse<SpaceData> = serde_json::from_str(&body)?;
    if api.code != 0 {
        return Err(DynamicsError::Api(api.code, api.message));
    }
    Ok(convert_page_data(api.data.unwrap_or_default()))
}

fn convert_page_data(data: SpaceData) -> DynamicsPage {
    let next_offset = if data.offset.is_empty() {
        None
    } else {
        Some(data.offset)
    };
    DynamicsPage {
        items: data
            .items
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<ItemRaw>(v) {
                Ok(raw) => Some(convert_item(raw)),
                Err(e) => {
                    tracing::warn!("跳过无法反序列化的 dynamic item: {e}");
                    None
                }
            })
            .collect(),
        has_more: data.has_more,
        next_offset,
    }
}

/// 拉取指定 UID 的 B 站用户空间动态。
///
/// 端点: <https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space>
///
/// **Env vars:** 通过 `.env` 文件或系统环境设置以下变量：
/// - `BILIBILI_COOKIE` — 可选的 B 站 cookie；未设置时自动获取游客 Cookie
/// - `BILIBILI_USER_AGENT` — HTTP 直连与 Chromium 后备共用的 UA；未设置时用内置 Chrome 147 Linux
///
/// 注意: B 站 web dynamic 接口风控较严，依赖 host 的 User-Agent + Referer +
/// WBI 签名和游客 Cookie。从被风控的 IP 调用仍可能返回 HTML 验证码页、
/// HTTP 412 或 `-352` 错误码；首次命中时会刷新匿名会话并重试一次。
pub async fn fetch_user_dynamics(
    uid: u64,
    offset: Option<&str>,
) -> Result<DynamicsPage, DynamicsError> {
    let session = web_session(false).await?;
    match fetch_page(uid, offset, &session).await {
        Err(error) if error.is_risk_control() => {
            tracing::warn!("Bilibili 动态请求触发风控，刷新匿名会话后重试一次: {error}");
            let refreshed = web_session(true).await?;
            match fetch_page(uid, offset, &refreshed).await {
                Err(error) if error.is_risk_control() => {
                    tracing::warn!(
                        "Bilibili 动态 HTTP 请求持续触发风控，切换 Chromium 后备: {error}"
                    );
                    match fetch_page_with_browser(uid, offset).await {
                        Err(error) if error.is_risk_control() => {
                            tracing::warn!(
                                "Bilibili Chromium 后备触发风控，刷新页面后重试一次: {error}"
                            );
                            fetch_page_with_browser(uid, offset).await
                        }
                        result => result,
                    }
                }
                result => result,
            }
        }
        result => result,
    }
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
        "DYNAMIC_TYPE_AV" => {
            convert_video(&id, &author, &raw.modules.module_dynamic).unwrap_or(DynamicItem::Other {
                id: id.clone(),
                author: author.clone(),
            })
        }
        "DYNAMIC_TYPE_DRAW" => {
            // 当前 B 站返回的图文动态用 MAJOR_TYPE_OPUS 承载；旧的 major.draw 已不再出现。
            convert_opus(&id, &author, &raw.modules.module_dynamic)
                .or_else(|| convert_draw(&id, &author, &raw.modules.module_dynamic))
                .unwrap_or(DynamicItem::Other {
                    id: id.clone(),
                    author: author.clone(),
                })
        }
        "DYNAMIC_TYPE_WORD" => convert_opus(&id, &author, &raw.modules.module_dynamic)
            .or_else(|| convert_word(&id, &author, &raw.modules.module_dynamic))
            .unwrap_or(DynamicItem::Other {
                id: id.clone(),
                author: author.clone(),
            }),
        "DYNAMIC_TYPE_ARTICLE" => convert_article(&id, &author, &raw.modules.module_dynamic)
            .unwrap_or(DynamicItem::Other {
                id: id.clone(),
                author: author.clone(),
            }),
        "DYNAMIC_TYPE_LIVE" | "DYNAMIC_TYPE_LIVE_RCMD" => {
            convert_live(&id, &author, &raw.modules.module_dynamic).unwrap_or(DynamicItem::Other {
                id: id.clone(),
                author: author.clone(),
            })
        }
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

    const TEST_UID: u64 = 546_195;

    #[tokio::test]
    #[ignore = "requires the public Bilibili API"]
    async fn anonymous_fetch_succeeds() {
        let page = fetch_user_dynamics(TEST_UID, None)
            .await
            .expect("anonymous feed/space request failed");
        println!(
            "uid={TEST_UID} has_more={} next_offset={:?} items={}",
            page.has_more,
            page.next_offset,
            page.items.len()
        );
        for item in page.items.iter().take(3) {
            println!("  {:?}", item);
        }
    }

    #[tokio::test]
    #[ignore = "requires the public Bilibili API"]
    async fn anonymous_fetch_has_items() {
        let page = fetch_user_dynamics(TEST_UID, None).await.unwrap();
        assert!(!page.items.is_empty(), "uid={TEST_UID} 应有动态");
    }

    #[tokio::test]
    #[ignore = "requires the public Bilibili API"]
    async fn anonymous_fetch_paginates() {
        let first = fetch_user_dynamics(TEST_UID, None).await.unwrap();
        if let Some(offset) = first.next_offset.as_deref() {
            let second = fetch_user_dynamics(TEST_UID, Some(offset)).await.unwrap();
            println!("page2 items={}", second.items.len());
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn user_agent_trims_and_falls_back() {
        assert_eq!(resolve_user_agent(None), DEFAULT_USER_AGENT);
        assert_eq!(resolve_user_agent(Some("")), DEFAULT_USER_AGENT);
        assert_eq!(resolve_user_agent(Some("  \t")), DEFAULT_USER_AGENT);
        assert_eq!(
            resolve_user_agent(Some("  CustomBot/1.0  ")),
            "CustomBot/1.0"
        );
    }

    #[test]
    fn url_builds_without_offset() {
        let url = build_space_url(1, None, "ea1db124af3c7062474693fa704f4ff8", 1_702_204_169);
        assert!(url.as_str().contains("host_mid=1"));
        assert!(url.as_str().contains("timezone_offset=-480"));
        assert!(url.as_str().contains("features=itemOpusStyle"));
        assert!(url.as_str().contains("w_rid="));
        assert!(url.as_str().contains("wts=1702204169"));
    }

    #[test]
    fn url_builds_with_offset() {
        let url = build_space_url(
            2,
            Some("abc"),
            "ea1db124af3c7062474693fa704f4ff8",
            1_702_204_169,
        );
        assert!(url.as_str().contains("host_mid=2"));
        assert!(url.as_str().contains("offset=abc"));
    }

    #[test]
    fn known_wbi_signing_input_matches() {
        let key = mixin_key(
            "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
            "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png",
        )
        .unwrap();
        assert_eq!(key, "ea1db124af3c7062474693fa704f4ff8");

        let params = BTreeMap::from([
            ("foo".into(), "114".into()),
            ("bar".into(), "514".into()),
            ("baz".into(), "1919810".into()),
        ]);
        assert_eq!(
            signed_query(params, &key, 1_702_204_169),
            "bar=514&baz=1919810&foo=114&wts=1702204169&\
             w_rid=6149fdadf571698ca7e6a567265cd0ee"
        );
    }

    #[test]
    fn guest_cookie_contains_only_required_values() {
        assert_eq!(
            guest_cookie("ABC-123infoc", 1_700_000_000).unwrap(),
            "buvid3=ABC-123infoc; b_nut=1700000000"
        );
        assert!(guest_cookie("bad; SESSDATA=injected", 1_700_000_000).is_err());
    }

    #[test]
    fn room_id_parses_basic_url() {
        assert_eq!(
            room_id_from_jump_url("https://live.bilibili.com/12345"),
            12345
        );
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
        assert_eq!(
            room_id_from_jump_url("https://live.bilibili.com/12345/"),
            12345
        );
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
