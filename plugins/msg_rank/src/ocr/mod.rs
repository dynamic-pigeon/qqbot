use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Error, Result};
use base64::{Engine, engine::general_purpose};
use moka::future::Cache;
use tracing::debug;

mod tencent;

use tencent::get_ocr;

/// 根目录 `config.toml` 的 `[ocr]`。两项都非空才启用腾讯云识别。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
struct OcrConfig {
    secret_id: String,
    secret_key: String,
}

impl OcrConfig {
    fn is_configured(&self) -> bool {
        !self.secret_id.trim().is_empty() && !self.secret_key.trim().is_empty()
    }
}

fn ocr_config() -> &'static OcrConfig {
    static CONFIG: LazyLock<OcrConfig> = LazyLock::new(|| {
        utils::config::parse("ocr").unwrap_or_else(|error| panic!("解析 [ocr] 配置失败: {error:#}"))
    });
    &CONFIG
}

fn ocr_image_hosts() -> &'static [&'static str] {
    static HOSTS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        let mut hosts = utils::QQ_IMAGE_HOSTS.to_vec();
        hosts.extend(["txmov2.a.yximgs.com", "yximgs.com", "qq.com"]);
        hosts
    });
    HOSTS.as_slice()
}

static OCR_MEMORY: LazyLock<OcrMemory> = LazyLock::new(OcrMemory::new);
static OCR_POOL: LazyLock<utils::BoundedPool> = LazyLock::new(|| utils::BoundedPool::new(4));

// 腾讯云请求体上限为 10 MiB，Base64 编码会把原始图片放大到约 4/3。
const MAX_OCR_IMAGE_BYTES: usize = 10 * 1024 * 1024 * 3 / 4;

struct OcrMemory {
    cache: Cache<String, Arc<String>>,
}

impl OcrMemory {
    fn new() -> Self {
        let cache = Cache::builder()
            .max_capacity(50)
            .time_to_live(Duration::from_secs(60 * 60 * 24))
            .time_to_idle(Duration::from_secs(60 * 64 * 10))
            .build();
        Self { cache }
    }

    async fn get_or_insert(&self, key: bytes::Bytes) -> Result<Arc<String>> {
        let key_sha256 = utils::sha256_hex(&key);
        if let Some(value) = self.cache.get(&key_sha256).await {
            return Ok(value);
        }

        let guard = self
            .cache
            .entry(key_sha256)
            .or_try_insert_with(async {
                let img_base64 = general_purpose::STANDARD.encode(&key);
                let v = get_ocr(&img_base64).await?;
                debug!("OCR cache miss, fetched from API");
                anyhow::Ok(Arc::new(v))
            })
            .await
            .map_err(|e| Error::msg(e.to_string()))?;

        let value = guard.value();

        if value.is_empty() {
            return Err(Error::msg("OCR result is empty"));
        }

        Ok(Arc::clone(value))
    }
}

/// 未配置腾讯云时跳过的告警只报一次，避免每条图片消息刷日志。
static OCR_MISSING_CONFIG_WARNED: AtomicBool = AtomicBool::new(false);

/// 对已校验过的图片 URL 做 OCR。未配置腾讯云时返回空串。
pub async fn ocr(img_url: &str) -> Result<Arc<String>> {
    // 未配置腾讯云时直接短路，不为注定失败的识别下载原图。
    if !ocr_config().is_configured() {
        if !OCR_MISSING_CONFIG_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!("未配置腾讯云 OCR，跳过图片文字识别");
        }
        return Ok(Arc::new(String::new()));
    }
    let _permit = OCR_POOL.acquire(Duration::from_secs(2)).await?;
    let img = get_img_bytes_from_url(img_url).await?;
    let result = OCR_MEMORY.get_or_insert(img).await?;

    Ok(result)
}

async fn get_img_bytes_from_url(img_url: &str) -> Result<bytes::Bytes> {
    let bytes = utils::download_image_limited(
        img_url,
        ocr_image_hosts(),
        MAX_OCR_IMAGE_BYTES,
        Duration::from_secs(10),
    )
    .await?;
    Ok(bytes::Bytes::from(bytes))
}
