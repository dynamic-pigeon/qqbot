use std::{sync::LazyLock, time::Duration};

use serde::Deserialize;
use utils::RateLimiter;

use crate::CLIENT;

#[derive(thiserror::Error, Debug)]
pub enum BvError {
    #[error("请求失败: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("解析返回体失败: {0}")]
    RequestBodyError(String),
    #[error("解析失败: {0}")]
    ParseFailed(&'static str),
    #[error("请求过于频繁，请稍后再试")]
    RateLimited,
}

#[derive(Deserialize)]
struct ApiRes {
    code: i32,
    message: String,
    data: Option<Data>,
}

#[derive(Deserialize)]
struct Data {
    title: String,
    pic: String,
    owner: Owner,
    stat: Stat,
    duration: u32,
}

#[derive(Deserialize)]
struct Owner {
    name: String,
}

#[derive(Deserialize)]
struct Stat {
    view: u32,
    coin: u32,
    like: u32,
    favorite: u32,
}

pub struct BvInfo {
    pub title: String,
    pub pic: bytes::Bytes,
    pub name: String,
    pub view: u32,
    pub coin: u32,
    pub like: u32,
    #[allow(dead_code)]
    pub duration: u32,
    pub url: String,
    pub favorite: u32,
}

impl ApiRes {
    async fn into_bv_info(self, url: String) -> Result<BvInfo, BvError> {
        if self.code != 0 {
            return Err(BvError::RequestBodyError(format!(
                "API请求失败: code={}, message={}",
                self.code, self.message
            )));
        }
        let data = self
            .data
            .ok_or_else(|| BvError::RequestBodyError("API返回缺少data字段".to_string()))?;
        let pic =
            crate::image::download_bili_image(&data.pic, 10 * 1024 * 1024, Duration::from_secs(10))
                .await
                .map(bytes::Bytes::from)
                .map_err(|e| BvError::RequestBodyError(format!("封面下载失败: {e}")))?;

        Ok(BvInfo {
            title: data.title,
            pic,
            name: data.owner.name,
            view: data.stat.view,
            coin: data.stat.coin,
            like: data.stat.like,
            duration: data.duration,
            favorite: data.stat.favorite,
            url,
        })
    }
}

/// 每群 5 秒内最多允许 3 次 BV 解析请求。
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(5);
const RATE_LIMIT_MAX_PER_WINDOW: usize = 3;

static RATE_LIMITER: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(RATE_LIMIT_WINDOW, RATE_LIMIT_MAX_PER_WINDOW));

fn check_rate_limit(group_id: i64) -> bool {
    RATE_LIMITER.try_acquire(group_id).is_ok()
}

static LONG_URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"https?://www\.bilibili\.com/video/(?P<bv>BV[0-9A-Za-z]{10})").unwrap()
});

static SHORT_URL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"https?://b23\.tv/(\w+)").unwrap());

pub async fn parse_url(url: &str, group_id: i64) -> Result<BvInfo, BvError> {
    match parse_long_url(url, group_id).await {
        Ok(info) => Ok(info),
        Err(BvError::ParseFailed(_)) => parse_short_url(url, group_id).await,
        Err(e) => Err(e),
    }
}

async fn parse_long_url(url: &str, group_id: i64) -> Result<BvInfo, BvError> {
    if let Some(caps) = LONG_URL_RE.captures(url) {
        let bv = &caps["bv"];
        parse_bv(bv, group_id).await
    } else {
        Err(BvError::ParseFailed("未匹配到长链接"))
    }
}

/// 判断 URL 是否指向 Bilibili 允许解析的域名。
///
/// 仅允许 `bilibili.com` / `www.bilibili.com` / `m.bilibili.com`，
/// 防止 b23.tv 短链重定向到内网或第三方站点造成 SSRF。
fn is_bilibili_url(url: &str) -> bool {
    [
        "https://www.bilibili.com/",
        "http://www.bilibili.com/",
        "https://bilibili.com/",
        "http://bilibili.com/",
        "https://m.bilibili.com/",
        "http://m.bilibili.com/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}

async fn parse_short_url(url: &str, group_id: i64) -> Result<BvInfo, BvError> {
    let caps = SHORT_URL_RE
        .captures(url)
        .ok_or(BvError::ParseFailed("未匹配到短链接"))?;
    let short_url = caps
        .get(0)
        .ok_or(BvError::ParseFailed("未匹配到短链接"))?
        .as_str();

    let resp = CLIENT.get(short_url).send().await?;

    // 已关闭自动重定向，短链必须返回 3xx 并携带 Location。
    if !resp.status().is_redirection() {
        return Err(BvError::ParseFailed("短链未返回重定向"));
    }

    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(BvError::ParseFailed("短链未返回 Location"))?;

    if !is_bilibili_url(location) {
        return Err(BvError::ParseFailed("短链重定向目标不是 Bilibili 域名"));
    }

    parse_long_url(location, group_id).await
}

async fn parse_bv(bv: &str, group_id: i64) -> Result<BvInfo, BvError> {
    if !check_rate_limit(group_id) {
        return Err(BvError::RateLimited);
    }

    let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bv);
    let res = CLIENT.get(&url).send().await?.json::<ApiRes>().await?;

    res.into_bv_info(format!("https://www.bilibili.com/video/{}", bv))
        .await
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    use super::*;

    // 集成测试统一使用 i64::MAX 附近的独立 group_id，
    // 避免与 `test_rate_limit_per_group` 的 i64::MAX / i64::MAX - 1 互相污染限流状态。
    const GROUP_INVALID: i64 = i64::MAX - 101;
    const GROUP_V_TEXT: i64 = i64::MAX - 102;
    const GROUP_HTTP_COVER: i64 = i64::MAX - 103;

    #[tokio::test]
    #[ignore = "依赖公网 B 站 API，非本地/CI 环境跳过"]
    async fn test_http_cover() {
        let url = "https://www.bilibili.com/video/BV1CVNU6xEom";
        let info = parse_url(url, GROUP_HTTP_COVER).await.unwrap();

        assert!(!info.pic.is_empty());
        assert_eq!(info.url, url);
    }

    #[tokio::test]
    async fn test_invalid() {
        let res = parse_url("not a bilibili url", GROUP_INVALID).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    #[ignore = "依赖公网 B 站 API，非本地/CI 环境跳过"]
    async fn test_v_text() {
        let txt = "【Fate/strange Fake】第13话（完结）[图片]UP主：花园字幕组
点赞：130 投币：47
收藏：72 观看：8359
https://www.bilibili.com/video/BV198XLBaEYp";

        let res = parse_url(txt, GROUP_V_TEXT).await;
        assert!(res.is_ok());
    }

    #[test]
    fn test_rate_limit_per_group() {
        // 每次取新的群号，避免静态限流器在同进程重复跑测试时被污染。
        static NEXT_GROUP: std::sync::atomic::AtomicI64 =
            std::sync::atomic::AtomicI64::new(i64::MAX);
        let group_a = NEXT_GROUP.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let group_b = NEXT_GROUP.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        assert!(check_rate_limit(group_a));
        assert!(check_rate_limit(group_a));
        assert!(check_rate_limit(group_a));
        assert!(!check_rate_limit(group_a));
        assert!(check_rate_limit(group_b));
    }
}
