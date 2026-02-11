use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::{Error, Result};
use base64::{Engine, engine::general_purpose};
use kovi::log::debug;
use moka::future::Cache;
use sha2::{Digest, Sha256};

mod tencent;

use tencent::get_ocr;

static OCR_MEMORY: LazyLock<OcrMemory> = LazyLock::new(OcrMemory::new);

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
        let key_sha256 = sha256_hex(&key);
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

/// 对图片URL进行OCR识别
///
/// # 参数
/// * `img_url` - 图片的URL地址
///
/// # 返回
/// * `Result<Arc<String>>` - OCR识别的文本结果
pub async fn ocr(img_url: &str) -> Result<Arc<String>> {
    let img = get_img_bytes_from_url(img_url).await?;
    let result = OCR_MEMORY.get_or_insert(img).await?;

    Ok(result)
}

/// 从URL获取图片
async fn get_img_bytes_from_url(img_url: &str) -> Result<bytes::Bytes> {
    let req = reqwest::get(img_url).await?;
    if !req.status().is_success() {
        return Err(anyhow::anyhow!("Failed to get image from URL"));
    }
    let bytes = req.bytes().await?;
    Ok(bytes)
}

/// SHA256哈希并转换为十六进制字符串
fn sha256_hex(data: &bytes::Bytes) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}
