use std::sync::LazyLock;

use serde::Deserialize;

use crate::CLIENT;

use error::BvError;

pub mod error;

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
    async fn into_bv_info(self, url: String) -> Result<BvInfo, error::BvError> {
        if self.code != 0 {
            return Err(BvError::RequestBodyError(format!(
                "API请求失败: code={}, message={}",
                self.code, self.message
            )));
        }
        let data = self
            .data
            .ok_or_else(|| BvError::RequestBodyError("API返回缺少data字段".to_string()))?;
        let pic = CLIENT.get(&data.pic).send().await?.bytes().await?;

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

static LONG_URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"https?://www\.bilibili\.com/video/(?P<bv>BV[0-9A-Za-z]{10})").unwrap()
});

static SHORT_URL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"https?://b23\.tv/(\w+)").unwrap());

pub async fn parse_url(url: &str) -> Result<BvInfo, error::BvError> {
    match parse_long_url(url).await {
        Ok(info) => Ok(info),
        Err(BvError::ParseFailed(_)) => parse_short_url(url).await,
        Err(e) => Err(e),
    }
}

async fn parse_long_url(url: &str) -> Result<BvInfo, error::BvError> {
    if let Some(caps) = LONG_URL_RE.captures(url) {
        let bv = &caps["bv"];
        return parse_bv(bv).await;
    }
    Err(BvError::ParseFailed("未匹配到长链接"))
}

async fn parse_short_url(url: &str) -> Result<BvInfo, error::BvError> {
    if !SHORT_URL_RE.is_match(url) {
        return Err(BvError::ParseFailed("未匹配到短链接"));
    }
    let resp = CLIENT.get(url).send().await?;
    let final_url = resp.url();
    parse_long_url(final_url.as_str()).await
}

async fn parse_bv(bv: &str) -> Result<BvInfo, error::BvError> {
    static CACHE: LazyLock<moka::future::Cache<String, ()>> = LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(20)
            .time_to_live(std::time::Duration::from_secs(5))
            .build()
    });

    let guard = CACHE.entry_by_ref(bv).or_insert_with(async {}).await;
    if !guard.is_fresh() {
        Err(BvError::Other("请求过于频繁，请稍后再试".to_string()))?;
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

    #[tokio::test]
    async fn test_long() {
        let url = "https://www.bilibili.com/video/BV198XLBaEYp";
        let info = parse_url(url).await.unwrap();
        println!("标题: {}", info.title);
        println!("作者: {}", info.name);
        println!("观看: {}", info.view);
        println!("评论: {}", info.coin);
        println!("点赞: {}", info.like);
    }

    #[tokio::test]
    async fn test_invalid() {
        let url = " https://www.bilibili.com/video/BV1PVdPBxEyr/?share_source=copy_web&vd_source=316166c47890d5daae6c8152b5f3e06f";
        let res = parse_url(url).await;
        assert!(res.is_err());
        println!("错误信息: {}", res.err().unwrap());
    }

    #[tokio::test]
    async fn test_v_text() {
        let txt = "【Fate/strange Fake】第13话（完结）[图片]UP主：花园字幕组
点赞：130 投币：47
收藏：72 观看：8359
https://www.bilibili.com/video/BV198XLBaEYp";

        let res = parse_url(txt).await;
        assert!(res.is_ok());
    }
}
