use std::fmt::Display;

use anyhow::Result;
use bytes::Bytes;

#[derive(serde::Deserialize)]
struct ApiRes {
    result: Vec<Card>,
}

#[derive(serde::Deserialize)]
pub struct Card {
    id: u64,
    cn_name: String,
    md_name: String,
    text: Text,
}

#[derive(serde::Deserialize)]
pub struct Text {
    types: String,
    desc: String,
}

impl Card {
    pub async fn fetch_image(&self) -> Result<Bytes> {
        fetch_img(self.id).await
    }
}

impl Display for Card {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "YGOPro译名: {}\nMaster Duel译名: {}\n\n{}\n\n{}",
            self.cn_name, self.md_name, self.text.types, self.text.desc
        )
    }
}

pub async fn fetch_card(name: &str) -> Result<Card> {
    let url = format!(
        "https://ygocdb.com/api/v0/?search={}",
        urlencoding::encode(name)
    );
    let resp: ApiRes = reqwest::get(&url).await?.json().await?;
    let ret = resp
        .result
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No card found for name: {}", name))?;
    Ok(ret)
}

async fn fetch_img(card_id: u64) -> Result<Bytes> {
    let url = format!("https://cdn.233.momobako.com/ygopro/pics/{}.jpg", card_id);
    let resp = reqwest::get(&url).await?;
    let bytes = resp.bytes().await?;
    Ok(bytes)
}
