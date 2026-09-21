use std::{fmt::Display, time::Duration};

use anyhow::Result;
use bytes::Bytes;
use kovi::serde_json;
use utils::http_client;

const MAX_API_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CARD_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct ApiRes {
    result: Vec<Card>,
}

#[derive(serde::Deserialize)]
pub struct Card {
    id: u64,
    cn_name: String,
    // Master Duel 未收录时接口会省略该字段，不能当成必填。
    #[serde(default)]
    md_name: Option<String>,
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
        let md_name = self
            .md_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or("无");
        write!(
            f,
            "YGOPro译名: {}\nMaster Duel译名: {}\n\n{}\n\n{}",
            self.cn_name, md_name, self.text.types, self.text.desc
        )
    }
}

/// 查询卡片，`Ok(None)` 表示查无此卡（正常的用户输入分支，不算内部错误）。
pub async fn fetch_card(name: &str) -> Result<Option<Card>> {
    let url = format!(
        "https://ygocdb.com/api/v0/?search={}",
        urlencoding::encode(name)
    );
    let response = http_client().get(&url).send().await?.error_for_status()?;
    let body = utils::read_response_limited(response, MAX_API_RESPONSE_BYTES).await?;
    parse_search_response(&body)
}

fn parse_search_response(body: &[u8]) -> Result<Option<Card>> {
    let resp: ApiRes = serde_json::from_slice(body)?;
    Ok(resp.result.into_iter().next())
}

async fn fetch_img(card_id: u64) -> Result<Bytes> {
    let url = format!("https://cdn.233.momobako.com/ygopro/pics/{}.jpg", card_id);
    let bytes = utils::download_image_limited(
        &url,
        &["cdn.233.momobako.com"],
        MAX_CARD_IMAGE_BYTES,
        Duration::from_secs(10),
    )
    .await?;
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_when_later_result_omits_md_name() {
        let body = r#"{
            "result": [
                {
                    "id": 89631139,
                    "cn_name": "青眼白龙",
                    "md_name": "青眼白龙",
                    "text": {"types": "[怪兽|通常]", "desc": "传说之龙"}
                },
                {
                    "id": 30397786,
                    "cn_name": "白色幻兽-青眼白龙",
                    "text": {"types": "[怪兽|效果]", "desc": "未收录"}
                }
            ]
        }"#;
        let card = parse_search_response(body.as_bytes())
            .unwrap()
            .expect("first card");
        assert_eq!(card.cn_name, "青眼白龙");
        assert_eq!(card.md_name.as_deref(), Some("青眼白龙"));
        let text = card.to_string();
        assert!(text.contains("Master Duel译名: 青眼白龙"));
    }

    #[test]
    fn parses_when_md_name_is_missing() {
        let body = r#"{
            "result": [
                {
                    "id": 13203964,
                    "cn_name": "完美电子多元驱动蛇·神龙",
                    "text": {"types": "[怪兽|效果|连接]", "desc": "连接怪兽1只以上"}
                }
            ]
        }"#;
        let card = parse_search_response(body.as_bytes())
            .unwrap()
            .expect("card");
        assert_eq!(card.cn_name, "完美电子多元驱动蛇·神龙");
        assert_eq!(card.md_name, None);
        assert!(card.to_string().contains("Master Duel译名: 无"));
    }
}
