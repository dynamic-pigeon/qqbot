//! 富媒体上传与 file_info 缓存。
//!
//! 官方发送图片是两步：先 `POST /v2/groups/{group_openid}/files`（或
//! `/v2/users/{openid}/files`）拿到 `file_info`，再以 msg_type=7 发送。
//! 直传（file_data base64）有 10MB 字节上限，更大的本地图走分块：
//! `upload_prepare` 拿分块预签名 URL，逐块 PUT 到 COS 并确认，complete
//! 换回同构的 file_info。file_info 带 ttl，按内容键缓存复用。

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use md5::Digest as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::token::{TokenError, TokenManager};
use crate::{ChatScope, PostError, UPLOAD_TIMEOUT, de_u64, post_json};

#[derive(Clone, Debug, PartialEq)]
pub enum MediaSource {
    Url(String),
    Bytes {
        data: Vec<u8>,
        filename: String,
    },
    /// `file://` 本地路径，上传前读成字节。
    FilePath(String),
}

impl MediaSource {
    /// 缓存键只认内容；本地图读成字节之后才有键。
    fn cache_key(&self) -> String {
        match self {
            MediaSource::Url(url) => format!("url:{url}"),
            MediaSource::Bytes { data, .. } => format!("bytes:{:016x}", fnv1a(data)),
            MediaSource::FilePath(_) => unreachable!("本地图在读成字节之后才有缓存键"),
        }
    }

    /// 读出本地图；URL 与字节原样返回。
    async fn read_local(&self) -> Result<Self, MediaError> {
        let MediaSource::FilePath(path) = self else {
            return Ok(self.clone());
        };
        let data = kovi::tokio::fs::read(path)
            .await
            .map_err(|error| MediaError {
                message: format!("读取 {path} 失败: {error}"),
            })?;
        let filename = Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image.png".to_owned());
        Ok(MediaSource::Bytes { data, filename })
    }
}

/// 内容寻址缓存键用的 64 位 FNV-1a。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, thiserror::Error)]
#[error("富媒体上传失败: {message}")]
pub struct MediaError {
    pub message: String,
}

impl From<reqwest::Error> for MediaError {
    fn from(error: reqwest::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<TokenError> for MediaError {
    fn from(error: TokenError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<PostError> for MediaError {
    fn from(error: PostError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// file_info 留 60 秒安全余量；ttl 为 0 表示可长期复用，按一小时计。
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const LONG_TTL_FALLBACK: Duration = Duration::from_secs(60 * 60);

/// 直传的字节上限，达到即改走分块。平台对 10,809,442 字节即回
/// err_code 40093011，门槛取十进制 10MB。
const DIRECT_UPLOAD_LIMIT: usize = 10_000_000;

/// upload_prepare 要求的 md5_10m 截断长度（官方 SDK 同款魔数）。
const MD5_10M_SIZE: usize = 10_002_432;

/// upload_part_finish 报此业务码表示分块尚未被平台确认。
const PART_PENDING_CODE: u64 = 40093001;
const PART_FINISH_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// prepare 没给 retry_timeout 时的确认重试时限。
const DEFAULT_PART_FINISH_RETRY_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MAX_PART_FINISH_RETRY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(4);

#[derive(Deserialize)]
struct UploadResponse {
    file_info: String,
    #[serde(default, deserialize_with = "de_u64")]
    ttl: u64,
}

#[derive(Deserialize)]
struct UploadPrepareResponse {
    upload_id: String,
    #[serde(deserialize_with = "de_u64")]
    block_size: u64,
    parts: Vec<UploadPart>,
    /// 平台给的分块确认重试时限（秒）。
    #[serde(default, deserialize_with = "de_u64")]
    retry_timeout: u64,
}

#[derive(Deserialize)]
struct UploadPart {
    #[serde(deserialize_with = "de_u64")]
    index: u64,
    presigned_url: String,
}

#[derive(Deserialize)]
struct MaterialPrepareResponse {
    upload_id: String,
    presigned_url: String,
}

#[derive(Deserialize)]
struct MaterialCompleteResponse {
    url: String,
    #[serde(default, deserialize_with = "de_u64")]
    ttl: u64,
}

pub struct MediaStore {
    inner: std::sync::Mutex<HashMap<String, (String, Instant)>>,
}

/// 嗅探图片 Content-Type；素材接口要求扩展名与声明一致。
pub(crate) fn sniff_content_type(data: &[u8]) -> (&'static str, &'static str) {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        ("image/png", "png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ("image/jpeg", "jpg")
    } else if data.starts_with(b"GIF8") {
        ("image/gif", "gif")
    } else if data.len() >= 12 && &data[8..12] == b"WEBP" {
        ("image/webp", "webp")
    } else {
        ("image/png", "png")
    }
}

/// 嵌图引用：URL + 像素宽高。markdown 图片语法必须带宽高，
/// 否则渲染成 `[img]` 纯文本。
#[derive(Clone)]
pub(crate) struct MaterialRef {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// 从图片字节嗅探像素尺寸（PNG / GIF / JPEG），webp 等少见格式返回 None。
pub(crate) fn sniff_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG：IHDR 固定在 16..24，大端。
    if data.len() >= 24 && data.starts_with(&[0x89, b'P', b'N', b'G']) {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }
    // GIF：逻辑屏幕描述符在 6..10，小端。
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }
    jpeg_dimensions(data)
}

/// JPEG：逐段扫 marker，第一个 SOFn 段里是高宽（大端）。
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut offset = 2;
    while offset + 9 < data.len() {
        if data[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = data[offset + 1];
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[offset + 2], data[offset + 3]]));
        let is_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_frame {
            let height = u32::from(u16::from_be_bytes([data[offset + 5], data[offset + 6]]));
            let width = u32::from(u16::from_be_bytes([data[offset + 7], data[offset + 8]]));
            return Some((width, height));
        }
        if marker == 0xDA {
            // 扫到扫描数据还没碰到帧头，结构异常。
            return None;
        }
        offset += 2 + length;
    }
    None
}

/// markdown 嵌图素材：本地图上传后拿 `qqbot-media://` 引用与像素尺寸。
/// 引用 90 天滑动有效，按内容缓存复用；单 bot 上限 100 张，
/// 满额时平台报错，由调用方回退为独立 media 消息。
pub(crate) async fn material_url(
    http: &reqwest::Client,
    token: &TokenManager,
    api_base: &str,
    store: &MediaStore,
    source: &MediaSource,
) -> Result<MaterialRef, MediaError> {
    let source = source.read_local().await?;
    let MediaSource::Bytes { data, .. } = source else {
        return Err(MediaError {
            message: "markdown 嵌图只接受本地图".to_owned(),
        });
    };

    let Some((width, height)) = sniff_dimensions(&data) else {
        return Err(MediaError {
            message: "无法嗅探图片尺寸，markdown 嵌图需要宽高".to_owned(),
        });
    };

    let cache_key = format!("material:bytes:{:016x}", fnv1a(&data));
    if let Some(cached) = store.lookup(&cache_key) {
        return Ok(MaterialRef {
            url: cached,
            width,
            height,
        });
    }

    let (content_type, extension) = sniff_content_type(&data);
    let md5 = md5_hex(&data);
    let prepare = json!({
        "file_name": format!("image.{extension}"),
        "file_size": data.len(),
        "content_type": content_type,
        "md5": md5,
        "sha1": sha1_hex(&data),
    });

    let authorization = token.authorization().await?;
    let text = post_upload(
        http,
        &authorization,
        format!("{api_base}/v2/media-images/prepare"),
        &prepare,
        "prepare",
    )
    .await?;
    let prepared: MaterialPrepareResponse = parse_response(&text, "prepare")?;

    put_presigned(http, &prepared.presigned_url, data, Some(content_type)).await?;

    let complete = json!({
        "upload_id": prepared.upload_id,
        "md5": md5,
    });
    let text = post_upload(
        http,
        &authorization,
        format!("{api_base}/v2/media-images/complete"),
        &complete,
        "complete",
    )
    .await?;
    let completed: MaterialCompleteResponse = parse_response(&text, "complete")?;

    // 引用 90 天滑动有效，缓存按 7 天收口。
    let lifetime = if completed.ttl == 0 {
        Duration::from_secs(7 * 24 * 60 * 60)
    } else {
        Duration::from_secs(completed.ttl).saturating_sub(EXPIRY_MARGIN)
    };
    store.store(&cache_key, &completed.url, lifetime);
    Ok(MaterialRef {
        url: completed.url,
        width,
        height,
    })
}

impl MediaStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn upload(
        &self,
        http: &reqwest::Client,
        token: &TokenManager,
        api_base: &str,
        scope: &ChatScope,
        source: MediaSource,
    ) -> Result<String, MediaError> {
        let source = source.read_local().await?;
        let cache_key = format!("{}:{}", scope.kind(), source.cache_key());
        if let Some(cached) = self.lookup(&cache_key) {
            return Ok(cached);
        }

        let (file_info, ttl) = match &source {
            MediaSource::Bytes { data, filename } if data.len() >= DIRECT_UPLOAD_LIMIT => {
                upload_chunked(http, token, api_base, scope, data, filename).await?
            }
            _ => upload_direct(http, token, api_base, scope, &source).await?,
        };

        let lifetime = if ttl == 0 {
            LONG_TTL_FALLBACK
        } else {
            Duration::from_secs(ttl).saturating_sub(EXPIRY_MARGIN)
        };
        self.store(&cache_key, &file_info, lifetime);
        Ok(file_info)
    }

    fn lookup(&self, key: &str) -> Option<String> {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((file_info, expire_at)) = cache.get(key) {
            if *expire_at > Instant::now() {
                return Some(file_info.clone());
            }
            cache.remove(key);
        }
        None
    }

    fn store(&self, key: &str, file_info: &str, lifetime: Duration) {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() > 1024 {
            cache.retain(|_, (_, expire_at)| *expire_at > Instant::now());
        }
        cache.insert(
            key.to_owned(),
            (file_info.to_owned(), Instant::now() + lifetime),
        );
    }
}

/// 直传：远程 URL 由平台代下载，本地图走 base64 file_data。
async fn upload_direct(
    http: &reqwest::Client,
    token: &TokenManager,
    api_base: &str,
    scope: &ChatScope,
    source: &MediaSource,
) -> Result<(String, u64), MediaError> {
    // srv_send_msg=false：只取 file_info 由发送接口发出，直发会占主动消息频次。
    let mut body = json!({ "file_type": 1, "srv_send_msg": false });
    match source {
        MediaSource::Url(url) => {
            body["url"] = json!(url);
        }
        MediaSource::Bytes { data, filename } => {
            body["file_data"] = json!(BASE64.encode(data));
            body["file_name"] = json!(filename);
        }
        MediaSource::FilePath(_) => unreachable!("本地图在 upload 入口已读成字节"),
    }

    let authorization = token.authorization().await?;
    let text = post_upload(
        http,
        &authorization,
        format!("{}{}/files", api_base, scope.base_path()),
        &body,
        "直传",
    )
    .await?;
    let response: UploadResponse = parse_response(&text, "直传")?;
    Ok((response.file_info, response.ttl))
}

/// 分块上传（官方 SDK 同款三段流程）：`upload_prepare` 提交三个哈希换分块
/// 预签名 URL；逐块 PUT 到 COS 后 `upload_part_finish` 确认；complete 复用
/// 直传的 /files 路径换回同构的 file_info。
async fn upload_chunked(
    http: &reqwest::Client,
    token: &TokenManager,
    api_base: &str,
    scope: &ChatScope,
    data: &[u8],
    filename: &str,
) -> Result<(String, u64), MediaError> {
    let (md5, sha1, md5_10m) = prepare_hashes(data);
    let authorization = token.authorization().await?;
    let prepare = json!({
        "file_type": 1,
        "file_name": filename,
        "file_size": data.len(),
        "md5": md5,
        "sha1": sha1,
        "md5_10m": md5_10m,
    });
    let text = post_upload(
        http,
        &authorization,
        format!("{}{}/upload_prepare", api_base, scope.base_path()),
        &prepare,
        "upload_prepare",
    )
    .await?;
    let prepared: UploadPrepareResponse = parse_response(&text, "upload_prepare")?;
    tracing::info!(
        "分块上传开始 size={} block_size={} parts={}",
        data.len(),
        prepared.block_size,
        prepared.parts.len()
    );

    let retry_timeout = match prepared.retry_timeout {
        0 => DEFAULT_PART_FINISH_RETRY_TIMEOUT,
        seconds => Duration::from_secs(seconds).min(MAX_PART_FINISH_RETRY_TIMEOUT),
    };
    let finish_url = format!("{}{}/upload_part_finish", api_base, scope.base_path());
    for part in &prepared.parts {
        let Some(range) = part_range(part.index, prepared.block_size, data.len()) else {
            return Err(MediaError {
                message: format!(
                    "分块 {} 区间非法: block_size={} size={}",
                    part.index,
                    prepared.block_size,
                    data.len()
                ),
            });
        };
        let bytes = &data[range];

        let url = &part.presigned_url;
        let put = || put_presigned(http, url, bytes.to_vec(), None);
        retry_backoff(put).await?;

        let finish = json!({
            "upload_id": prepared.upload_id,
            "part_index": part.index,
            "block_size": bytes.len(),
            "md5": md5_hex(bytes),
        });
        finish_part(http, &authorization, &finish_url, &finish, retry_timeout)
            .await
            .map_err(|error| MediaError {
                message: format!("upload_part_finish 失败（第 {} 块）: {error}", part.index),
            })?;
    }

    let complete = json!({ "upload_id": prepared.upload_id });
    let url = format!("{}{}/files", api_base, scope.base_path());
    let post = || post_upload(http, &authorization, url.clone(), &complete, "complete");
    let text = retry_backoff(post).await?;
    let response: UploadResponse = parse_response(&text, "complete")?;
    Ok((response.file_info, response.ttl))
}

/// 分块在文件里的字节区间；`index` 从 1 起。
fn part_range(index: u64, block_size: u64, len: usize) -> Option<std::ops::Range<usize>> {
    let block_size = usize::try_from(block_size).ok()?;
    let start = usize::try_from(index.checked_sub(1)?)
        .ok()?
        .checked_mul(block_size)?;
    let end = start.checked_add(block_size)?.min(len);
    (start < end).then_some(start..end)
}

/// 分块确认。业务码 40093001 表示平台尚未确认该分块，在 `retry_timeout`
/// 内每秒重试。
async fn finish_part(
    http: &reqwest::Client,
    authorization: &str,
    url: &str,
    body: &Value,
    retry_timeout: Duration,
) -> Result<(), PostError> {
    let deadline = Instant::now() + retry_timeout;
    loop {
        match post_json(http, authorization, url.to_owned(), body, UPLOAD_TIMEOUT).await {
            Ok(_) => return Ok(()),
            Err(error)
                if error.biz_code() == Some(PART_PENDING_CODE) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
        kovi::tokio::time::sleep(PART_FINISH_RETRY_INTERVAL).await;
    }
}

/// 上传步骤的 POST：上传类超时，失败信息带步骤名。
async fn post_upload(
    http: &reqwest::Client,
    authorization: &str,
    url: String,
    body: &Value,
    step: &str,
) -> Result<String, MediaError> {
    post_json(http, authorization, url, body, UPLOAD_TIMEOUT)
        .await
        .map_err(|error| MediaError {
            message: format!("{step}: {error}"),
        })
}

fn parse_response<'a, T: Deserialize<'a>>(text: &'a str, step: &str) -> Result<T, MediaError> {
    serde_json::from_str(text).map_err(|error| MediaError {
        message: format!("{step} 响应解析失败（{text}）: {error}"),
    })
}

/// 失败按 2s / 4s 退避重试两次。
async fn retry_backoff<T, F, Fut>(mut attempt: F) -> Result<T, MediaError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, MediaError>>,
{
    let mut delay = RETRY_INITIAL_DELAY;
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if delay > RETRY_MAX_DELAY {
            return Err(error);
        }
        tracing::warn!("上传步骤失败，{} 秒后重试: {error}", delay.as_secs());
        kovi::tokio::time::sleep(delay).await;
        delay *= 2;
    }
}

/// PUT 字节到预签名 URL（不带 Authorization，COS 只认预签名）。
async fn put_presigned(
    http: &reqwest::Client,
    url: &str,
    bytes: Vec<u8>,
    content_type: Option<&str>,
) -> Result<(), MediaError> {
    let mut request = http.put(url).timeout(UPLOAD_TIMEOUT).body(bytes);
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    let response = request.send().await.map_err(|error| MediaError {
        message: format!("预签名 PUT 失败: {error}"),
    })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(MediaError {
        message: format!("预签名 PUT HTTP {status}: {body}"),
    })
}

/// upload_prepare 的三件套哈希：全量 md5 / sha1 / 前 `MD5_10M_SIZE`
/// 字节的 md5（不足时等于全量 md5）。
fn prepare_hashes(data: &[u8]) -> (String, String, String) {
    let md5 = md5_hex(data);
    let sha1 = sha1_hex(data);
    let md5_10m = if data.len() > MD5_10M_SIZE {
        md5_hex(&data[..MD5_10M_SIZE])
    } else {
        md5.clone()
    };
    (md5, sha1, md5_10m)
}

fn md5_hex(bytes: &[u8]) -> String {
    to_hex(&md5::Md5::digest(bytes))
}

fn sha1_hex(bytes: &[u8]) -> String {
    to_hex(&sha1::Sha1::digest(bytes))
}

fn to_hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare三件套哈希() {
        // 不足 10M 时 md5_10m 等于全量 md5。
        let (md5, sha1, md5_10m) = prepare_hashes(b"hello");
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(sha1, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        assert_eq!(md5_10m, md5);
    }

    #[test]
    fn prepare响应数字字段收字符串形态() {
        let parsed: UploadPrepareResponse = serde_json::from_str(
            r#"{"upload_id":"U1","block_size":"10485760","parts":[{"index":"1","presigned_url":"https://cos.example/p1"}],"retry_timeout":"120"}"#,
        )
        .unwrap();
        assert_eq!(parsed.block_size, 10_485_760);
        assert_eq!(parsed.parts[0].index, 1);
        assert_eq!(parsed.parts[0].presigned_url, "https://cos.example/p1");
        assert_eq!(parsed.retry_timeout, 120);
    }

    #[test]
    fn 分块区间按序号顺延且末块截到文件尾() {
        let range = |index, block_size| part_range(index, block_size, 25);
        assert_eq!(range(1, 10), Some(0..10));
        assert_eq!(range(2, 10), Some(10..20));
        assert_eq!(range(3, 10), Some(20..25));
        // 序号越界、序号为 0、块大小为 0 都没有合法区间。
        assert_eq!(range(4, 10), None);
        assert_eq!(range(0, 10), None);
        assert_eq!(range(1, 0), None);
        assert_eq!(range(u64::MAX, u64::MAX), None);
    }

    #[test]
    fn 业务错误码取响应体里的err_code或code() {
        let biz_code = |body: &str| {
            PostError::Rejected {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: body.to_owned(),
            }
            .biz_code()
        };
        assert_eq!(biz_code(r#"{"err_code":40093001}"#), Some(40093001));
        assert_eq!(biz_code(r#"{"code":"40093002"}"#), Some(40093002));
        assert_eq!(biz_code("not json"), None);
    }

    #[test]
    fn 图片尺寸嗅探() {
        // PNG 的宽高在 IHDR（大端），GIF 的在逻辑屏幕描述符（小端）。
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x01\x00\x00\x00\x00\xc8";
        let gif = b"GIF89a\x40\x01\xd0\x00";
        assert_eq!(sniff_dimensions(png), Some((256, 200)));
        assert_eq!(sniff_dimensions(gif), Some((320, 208)));
        assert_eq!(sniff_dimensions(b"junk"), None);
    }

    #[test]
    fn 图片类型嗅探() {
        assert_eq!(sniff_content_type(b"\x89PNG"), ("image/png", "png"));
        assert_eq!(
            sniff_content_type(b"\xff\xd8\xff\xe0"),
            ("image/jpeg", "jpg")
        );
        assert_eq!(sniff_content_type(b"GIF89a"), ("image/gif", "gif"));
        // 未知内容按 png 兜底，交给平台校验。
        assert_eq!(sniff_content_type(b"junk"), ("image/png", "png"));
    }
}
