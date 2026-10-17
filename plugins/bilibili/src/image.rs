use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;

use crate::dynamics::ALLOWED_BILI_HOSTS;

/// 下载 B 站图片字节并聚合为 [`Bytes`]，供消息段构建直接引用。
pub(crate) async fn fetch_bili_image_bytes(
    url: &str,
    max_bytes: usize,
    request_timeout: Duration,
) -> Result<Bytes> {
    let bytes = download_bili_image(url, max_bytes, request_timeout).await?;
    Ok(Bytes::from(bytes))
}

pub(crate) async fn download_bili_image(
    url: &str,
    max_bytes: usize,
    request_timeout: Duration,
) -> Result<Vec<u8>> {
    let url = normalize_bili_image_url(url)?;
    utils::download_image_limited(url.as_str(), ALLOWED_BILI_HOSTS, max_bytes, request_timeout)
        .await
}

/// 校验并归一化 B 站图片 URL（http 升 https、host 白名单），
/// 供消息段直接引用、由 onebot 端下载。
pub(crate) fn bili_image_url(url: &str) -> Result<String> {
    let url = normalize_bili_image_url(url)?;
    utils::validate_image_url(url.as_str(), ALLOWED_BILI_HOSTS)?;
    Ok(url.to_string())
}

fn normalize_bili_image_url(url: &str) -> Result<reqwest::Url> {
    // 部分接口返回 HTTP 或协议相对 CDN 地址，对应 CDN 支持 HTTPS，统一升到 https。
    let url = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    };
    let mut parsed = reqwest::Url::parse(&url).context("Bilibili 图片 URL 解析失败")?;

    match parsed.scheme() {
        "http" => parsed
            .set_scheme("https")
            .map_err(|_| anyhow::anyhow!("Bilibili 图片 URL 无法升级为 HTTPS"))?,
        "https" => {}
        scheme => return Err(anyhow::anyhow!("Bilibili 图片 URL scheme 不支持: {scheme}")),
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_http_image_url_to_https() {
        let url = normalize_bili_image_url(
            "http://i0.hdslb.com/bfs/archive/0a4b5d5f456a854b7f9890e93ff63604d02056ef.jpg",
        )
        .unwrap();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("i0.hdslb.com"));
        assert!(utils::validate_image_url(url.as_str(), ALLOWED_BILI_HOSTS).is_ok());
    }

    #[test]
    fn normalizes_scheme_relative_image_url() {
        let url = normalize_bili_image_url("//i0.hdslb.com/bfs/archive/cover.jpg").unwrap();

        assert_eq!(url.as_str(), "https://i0.hdslb.com/bfs/archive/cover.jpg");
    }

    #[test]
    fn rejects_unsupported_image_url_scheme() {
        assert!(normalize_bili_image_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn bili_image_url_upgrades_and_checks_host() {
        assert_eq!(
            bili_image_url("http://i0.hdslb.com/bfs/archive/cover.jpg").unwrap(),
            "https://i0.hdslb.com/bfs/archive/cover.jpg"
        );
        assert!(bili_image_url("https://evil.com/x.jpg").is_err());
    }
}
