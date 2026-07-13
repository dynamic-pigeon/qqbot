use std::{fmt::Display, sync::LazyLock, time::Duration};

use anyhow::Result;
use bytes::Bytes;

const MAX_API_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CARD_IMAGE_BYTES: usize = 8 * 1024 * 1024;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("hardcoded reqwest client configuration must be valid")
});

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
    let response = HTTP_CLIENT.get(&url).send().await?;
    let body = utils::read_response_limited(response, MAX_API_RESPONSE_BYTES).await?;
    let resp: ApiRes = serde_json::from_slice(&body)?;
    let ret = resp
        .result
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No card found for name: {}", name))?;
    Ok(ret)
}

async fn fetch_img(card_id: u64) -> Result<Bytes> {
    let url = format!("https://cdn.233.momobako.com/ygopro/pics/{}.jpg", card_id);
    let bytes = utils::download_image_limited(
        &url,
        &["cdn.233.momobako.com"],
        true,
        MAX_CARD_IMAGE_BYTES,
        Duration::from_secs(10),
    )
    .await?;
    Ok(Bytes::from(bytes))
}
