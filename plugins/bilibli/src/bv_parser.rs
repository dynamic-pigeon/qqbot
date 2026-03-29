use std::sync::{Arc, LazyLock};

use anyhow::Result;

use serde::Deserialize;

#[derive(Deserialize)]
struct ApiRes {
    code: i32,
    message: String,
    data: Data,
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
    async fn to_bv_info(self, url: String) -> Result<BvInfo> {
        if self.code != 0 {
            anyhow::bail!("请求失败: {}", self.message);
        }
        let pic = reqwest::get(&self.data.pic).await?.bytes().await?;

        Ok(BvInfo {
            title: self.data.title,
            pic,
            name: self.data.owner.name,
            view: self.data.stat.view,
            coin: self.data.stat.coin,
            like: self.data.stat.like,
            duration: self.data.duration,
            favorite: self.data.stat.favorite,
            url,
        })
    }
}

pub async fn parse_url(url: &str) -> Result<Arc<BvInfo>> {
    match parse_long_url(url).await {
        Ok(info) => Ok(info),
        Err(_) => parse_short_url(url).await,
    }
}

async fn parse_long_url(url: &str) -> Result<Arc<BvInfo>> {
    let re = regex::Regex::new(r"https?://www\.bilibili\.com/video/(?P<bv>BV\w+)").unwrap();
    if let Some(caps) = re.captures(url) {
        let bv = &caps["bv"];
        return parse_bv(bv).await;
    }
    anyhow::bail!("未匹配到长链接");
}

async fn parse_short_url(url: &str) -> Result<Arc<BvInfo>> {
    let re = regex::Regex::new(r"https?://b23\.tv/(\w+)").unwrap();
    if !re.is_match(url) {
        anyhow::bail!("未匹配到短链接");
    }
    let resp = reqwest::get(url).await?;
    let final_url = resp.url();
    parse_long_url(final_url.as_str()).await
}

async fn parse_bv(bv: &str) -> Result<Arc<BvInfo>> {
    static CACHE: LazyLock<moka::future::Cache<String, Arc<BvInfo>>> = LazyLock::new(|| {
        moka::future::Cache::builder()
            .max_capacity(20)
            .time_to_live(std::time::Duration::from_secs(60 * 60 * 24))
            .build()
    });

    let guard = CACHE
        .entry_by_ref(bv)
        .or_try_insert_with(async {
            let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bv);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::REFERER,
                reqwest::header::HeaderValue::from_static("https://www.bilibili.com/"),
            );
            let client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
                .default_headers(headers)
                .build()
                .unwrap();
            let res = client
                .get(&url)
                .send()
                .await
                .unwrap()
                .json::<ApiRes>()
                .await?;

            res.to_bv_info(format!("https://www.bilibili.com/video/{}", bv))
                .await
                .map(Arc::new)
        })
        .await
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;

    let info = guard.value();
    Ok(Arc::clone(info))
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
        let url = "https://www.bilibili.com/video/invalid";
        let res = parse_url(url).await;
        assert!(res.is_err());
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
