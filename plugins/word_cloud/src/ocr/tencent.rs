// 腾讯云API签名v3 Rust实现
// 本代码基于腾讯云API签名v3文档实现: https://cloud.tencent.com/document/product/213/30654
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use kovi::{
    log::warn,
    serde_json::{self, Value},
};
use sha2::{Digest, Sha256};

use crate::read_config;

type HmacSha256 = Hmac<Sha256>;

/// 获取UTC日期字符串 (YYYY-MM-DD格式)
fn get_date(timestamp: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(timestamp, 0).unwrap();
    dt.format("%Y-%m-%d").to_string()
}

/// SHA256哈希并转换为十六进制字符串
fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// HMAC-SHA256计算
fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// 执行腾讯云OCR API调用
pub(crate) async fn get_ocr(img_base64: &str) -> Result<String> {
    let config = read_config().await;

    let config = match &config.tencent {
        Some(cfg) => cfg,
        None => {
            warn!("Tencent Cloud OCR configuration is missing");
            return Ok(String::new());
        }
    };
    // 密钥信息从配置读取
    let secret_id = &config.secret_id;
    let secret_key = &config.secret_key;

    let service = "ocr";
    let host = "ocr.tencentcloudapi.com";
    let region = "";
    let action = if rand::random_range(0..=1) == 0 {
        "GeneralBasicOCR" // 通用印刷体识别
    } else {
        "GeneralAccurateOCR" // 通用印刷体识别（高精度）
    };
    let version = "2018-11-19";

    // 获取当前时间戳
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let date = get_date(timestamp);

    // ************* 步骤 1：拼接规范请求串 *************
    let http_request_method = "POST";
    let canonical_uri = "/";
    let canonical_query_string = "";
    let canonical_headers = format!(
        "content-type:application/json; charset=utf-8\nhost:{}\n",
        host
    );
    let signed_headers = "content-type;host";

    // 构造请求体
    let payload = serde_json::json!({
        "ImageBase64": img_base64
    })
    .to_string();

    let hashed_request_payload = sha256_hex(&payload);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        http_request_method,
        canonical_uri,
        canonical_query_string,
        canonical_headers,
        signed_headers,
        hashed_request_payload
    );

    // ************* 步骤 2：拼接待签名字符串 *************
    let algorithm = "TC3-HMAC-SHA256";
    let request_timestamp = timestamp.to_string();
    let credential_scope = format!("{}/{}/tc3_request", date, service);
    let hashed_canonical_request = sha256_hex(&canonical_request);
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        algorithm, request_timestamp, credential_scope, hashed_canonical_request
    );

    // ************* 步骤 3：计算签名 *************
    let secret_date = hmac_sha256(format!("TC3{}", secret_key).as_bytes(), &date);
    let secret_service = hmac_sha256(&secret_date, service);
    let secret_signing = hmac_sha256(&secret_service, "tc3_request");
    let signature = hex::encode(hmac_sha256(&secret_signing, &string_to_sign));

    // ************* 步骤 4：拼接 Authorization *************
    let authorization = format!(
        "{} Credential={}/{}, SignedHeaders={}, Signature={}",
        algorithm, secret_id, credential_scope, signed_headers, signature
    );

    // ************* 步骤 5：构造并发起请求 *************
    let url = format!("https://{}", host);
    let client = reqwest::Client::new();

    let mut request = client
        .post(&url)
        .header("Authorization", authorization)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Host", host)
        .header("X-TC-Action", action)
        .header("X-TC-Timestamp", request_timestamp)
        .header("X-TC-Version", version)
        .header("X-TC-Region", region);

    request = request.header("X-TC-Token", "");

    let response = request.body(payload).send().await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "OCR API request failed with status: {}",
            response.status()
        ));
    }

    let response_json: Value = response.json().await?;

    // 解析响应
    if let Some(response_obj) = response_json.get("Response") {
        // 检查是否有错误
        if let Some(error) = response_obj.get("Error") {
            return Err(anyhow::anyhow!(
                "OCR API error: {}",
                error
                    .get("Message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error")
            ));
        }

        // 提取文本检测结果
        let text_detections = response_obj
            .get("TextDetections")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("Invalid response format: missing TextDetections"))?;

        let result = text_detections
            .iter()
            .filter_map(|item| {
                item.get("DetectedText")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect::<Vec<_>>()
            .join(" ");

        Ok(result)
    } else {
        Err(anyhow::anyhow!(
            "Invalid response format: missing Response field"
        ))
    }
}
