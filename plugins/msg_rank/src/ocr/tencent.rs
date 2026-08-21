// 腾讯云 API 签名 v3：https://cloud.tencent.com/document/product/213/30654
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use kovi::serde_json::{self, Value};
use sha2::{Digest, Sha256};

use crate::{HTTP_CLIENT, hex_encode};

type HmacSha256 = Hmac<Sha256>;

fn get_date(timestamp: i64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(timestamp, 0).unwrap();
    dt.format("%Y-%m-%d").to_string()
}

fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub(crate) async fn get_ocr(img_base64: &str) -> Result<String> {
    let config = super::ocr_config();
    let secret_id = config.secret_id.as_str();
    let secret_key = config.secret_key.as_str();

    let service = "ocr";
    let host = "ocr.tencentcloudapi.com";
    let region = "";
    let action = if rand::random::<bool>() {
        "GeneralBasicOCR" // 通用印刷体识别
    } else {
        "GeneralAccurateOCR" // 通用印刷体识别（高精度）
    };
    let version = "2018-11-19";

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let date = get_date(timestamp);

    let http_request_method = "POST";
    let canonical_uri = "/";
    let canonical_query_string = "";
    let canonical_headers = format!(
        "content-type:application/json; charset=utf-8\nhost:{}\n",
        host
    );
    let signed_headers = "content-type;host";

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

    let algorithm = "TC3-HMAC-SHA256";
    let request_timestamp = timestamp.to_string();
    let credential_scope = format!("{}/{}/tc3_request", date, service);
    let hashed_canonical_request = sha256_hex(&canonical_request);
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        algorithm, request_timestamp, credential_scope, hashed_canonical_request
    );

    let secret_date = hmac_sha256(format!("TC3{}", secret_key).as_bytes(), &date);
    let secret_service = hmac_sha256(&secret_date, service);
    let secret_signing = hmac_sha256(&secret_service, "tc3_request");
    let signature = hex_encode(&hmac_sha256(&secret_signing, &string_to_sign));

    let authorization = format!(
        "{} Credential={}/{}, SignedHeaders={}, Signature={}",
        algorithm, secret_id, credential_scope, signed_headers, signature
    );

    let url = format!("https://{}", host);

    let mut request = HTTP_CLIENT
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

    if let Some(response_obj) = response_json.get("Response") {
        if let Some(error) = response_obj.get("Error") {
            return Err(anyhow::anyhow!(
                "OCR API error: {}",
                error
                    .get("Message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error")
            ));
        }

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
