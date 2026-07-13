//! Integration tests for `utils::safe_url::validate_image_url` (SSRF defense).
//!
//! 覆盖范围：
//! - 白名单内的 host 应该被接受（含子域名后缀匹配）
//! - 非白名单 host / 非 https scheme / 畸形输入 应该被拒绝
//! - 私有 IP（IPv4 10/8、172.16/12、192.168/16、169.254/16、loopback、unspecified、
//!   broadcast、multicast；IPv6 loopback、unique-local、link-local、IPv4-mapped IPv6）
//!   应该被拒绝，防止攻击者用公网域名解析到内网 IP 后访问内部服务。

use utils::safe_url::{is_public_ip, validate_image_url};

/// 测试用 host 白名单。`utils` 只暴露校验机制（mechanism），
/// 不再持有具体 host 列表（policy 在调用方）；
/// 这里用一组样本数据覆盖 validator 的 hostname 匹配逻辑。
const TEST_HOSTS: &[&str] = &[
    "gchat.qpic.cn",
    "multimedia.nt.qq.com.cn",
    "c2cpicdw.qpic.cn",
];

#[test]
fn accepts_whitelisted_hosts() {
    assert!(validate_image_url("https://gchat.qpic.cn/abc", TEST_HOSTS).is_ok());
    assert!(validate_image_url("https://multimedia.nt.qq.com.cn/abc", TEST_HOSTS).is_ok());
    assert!(validate_image_url("https://c2cpicdw.qpic.cn/abc", TEST_HOSTS).is_ok());
    // 子域名匹配：`sub.gchat.qpic.cn` 应匹配 `gchat.qpic.cn` 后缀
    assert!(validate_image_url("https://sub.gchat.qpic.cn/abc", TEST_HOSTS).is_ok());
}

#[test]
fn rejects_http_scheme() {
    assert!(validate_image_url("http://gchat.qpic.cn/abc", TEST_HOSTS).is_err());
}

#[test]
fn rejects_non_whitelisted_hosts() {
    assert!(validate_image_url("https://evil.com/x", TEST_HOSTS).is_err());
    assert!(validate_image_url("https://8.8.8.8/x", TEST_HOSTS).is_err());
    // 子域名后缀混淆攻击：evil.com 不能通过 .qq.com 后缀检查
    assert!(validate_image_url("https://qq.com.evil.com/x", TEST_HOSTS).is_err());
}

#[test]
fn rejects_ipv4_attack_addresses() {
    // loopback 127.0.0.0/8
    assert!(validate_image_url("https://127.0.0.1/x", TEST_HOSTS).is_err());
    assert!(validate_image_url("https://127.255.255.254/x", TEST_HOSTS).is_err());
    // 10.0.0.0/8
    assert!(validate_image_url("https://10.0.0.1/x", TEST_HOSTS).is_err());
    // 172.16.0.0/12
    assert!(validate_image_url("https://172.16.0.1/x", TEST_HOSTS).is_err());
    assert!(validate_image_url("https://172.31.255.254/x", TEST_HOSTS).is_err());
    // 192.168.0.0/16
    assert!(validate_image_url("https://192.168.1.1/x", TEST_HOSTS).is_err());
    // 169.254.0.0/16 link-local（AWS metadata 169.254.169.254 在此范围）
    assert!(validate_image_url("https://169.254.169.254/x", TEST_HOSTS).is_err());
    // unspecified / broadcast / multicast
    assert!(validate_image_url("https://0.0.0.0/x", TEST_HOSTS).is_err());
    assert!(validate_image_url("https://255.255.255.255/x", TEST_HOSTS).is_err());
    assert!(validate_image_url("https://224.0.0.1/x", TEST_HOSTS).is_err());
}

#[test]
fn rejects_ipv6_attack_addresses() {
    // loopback
    assert!(validate_image_url("https://[::1]/x", TEST_HOSTS).is_err());
    // unique-local (fc00::/7)
    assert!(validate_image_url("https://[fc00::1]/x", TEST_HOSTS).is_err());
    // link-local (fe80::/10)
    assert!(validate_image_url("https://[fe80::1]/x", TEST_HOSTS).is_err());
    // IPv4-mapped IPv6: ::ffff:127.0.0.1 应被当作 127.0.0.1 拒绝
    assert!(validate_image_url("https://[::ffff:127.0.0.1]/x", TEST_HOSTS).is_err());
    // ::ffff:10.0.0.1 应被当作 10.0.0.1 拒绝
    assert!(validate_image_url("https://[::ffff:10.0.0.1]/x", TEST_HOSTS).is_err());
    assert!(!is_public_ip(&"2001:db8::1".parse().unwrap()));
    assert!(is_public_ip(&"2606:4700:4700::1111".parse().unwrap()));
}

#[test]
fn rejects_malformed_input() {
    assert!(validate_image_url("not a url", TEST_HOSTS).is_err());
    assert!(validate_image_url("", TEST_HOSTS).is_err());
    // 危险 scheme 即使没有 host 也应被拒绝
    assert!(validate_image_url("javascript:alert(1)", TEST_HOSTS).is_err());
    assert!(validate_image_url("file:///etc/passwd", TEST_HOSTS).is_err());
}
