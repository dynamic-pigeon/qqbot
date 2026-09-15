use base64::Engine as _;
use sha2::{Digest, Sha256};

/// OneBot 图片地址：`base64://` + 标准 Base64。
pub fn base64_image(bytes: &[u8]) -> String {
    const PREFIX: &str = "base64://";
    let mut uri = String::with_capacity(PREFIX.len() + bytes.len().div_ceil(3) * 4);
    uri.push_str(PREFIX);
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut uri);
    uri
}

/// 小写十六进制编码。sha256 和 HMAC 签名共用，避免再引 hex crate。
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}
