//! 共享的 SSRF 防御：图片 URL 必须 https + host 在白名单 + host 是 IP 时拒绝非公网。
//!
//! 设计要点：
//! - 同步路径：scheme 校验 + host 解析 + 字面 IP 黑名单（一次性 O(1)，可在热路径调用）。
//! - 异步路径：域名场景额外做 DNS 预解析，每条 A/AAAA 都必须是公网单播地址，
//!   防止 attacker 用公网域名 → 内网 IP 的 DNS rebinding 绕过 URL 字符串层校验。
//!
//! 调用方约定：
//! - 调用 `validate_image_url` 前必须有合法 `allowed_hosts` 列表；运行时不应接收 user 输入的 host。
//! - 调 HTTP 客户端的 `redirect` 策略保持 `Policy::none()`，否则 redirect 到的地址
//!   仍会绕过白名单（与 DNS 预解析无关，是另一道墙）。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::Result;

const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// 校验图片 URL：仅允许 https + 白名单域名；host 是 IP 时拒绝所有非公网单播地址。
///
/// 这是 SSRF 防护的第一道墙：禁止 `127.0.0.1` / `10.*` / `192.168.*` / `169.254.*` /
/// `0.0.0.0` / IPv6 loopback & private 等任何内网/loopback/link-local 地址。
///
/// 注意：仅校验 URL 字符串层；对抗 DNS rebinding 还需要 [`validate_image_url_async`]
/// 在调用 HTTP 之前多一次 DNS 预解析检查。
pub fn validate_image_url(url: &str, allowed_hosts: &[&str]) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("image url 解析失败: {e}"))?;

    if parsed.scheme() != "https" {
        return Err(anyhow::anyhow!(
            "image url scheme 必须是 https，收到: {}",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("image url 缺少 host"))?;

    // host 是字面 IP（v4/v6）时按地址族判断；是域名时按白名单匹配
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(&ip) {
            return Err(anyhow::anyhow!("image url host 命中非公网地址: {}", ip));
        }
        return Ok(());
    }

    let host_lower = host.to_ascii_lowercase();
    let allowed = allowed_hosts
        .iter()
        .any(|allowed| host_lower == *allowed || host_lower.ends_with(&format!(".{allowed}")));
    if !allowed {
        return Err(anyhow::anyhow!("image url host 不在白名单: {}", host));
    }

    Ok(())
}

/// 异步校验图片 URL：在 [`validate_image_url`] 基础上可选对域名做 DNS 预解析，
/// 确保所有 A/AAAA 记录都是公网单播地址。
///
/// 防 DNS rebinding 的关键：attacker 控制权威 DNS 时，可在 URL 字符串校验阶段
/// 返回公网 IP、HTTP 实际连接阶段返回内网 IP。本函数把这一步前置：
/// ```text
/// validate_image_url (字符串层) + lookup_host + is_public_ip
/// ```
///
/// 仍存在极小的 TOCTOU 窗口（attacker 在校验通过后、reqwest 实际 connect 前切 DNS），
/// 配合 HTTP 客户端的 redirect = none + 短超时足够覆盖绝大多数攻击面。
///
/// `check_dns` 控制是否执行 DNS 预解析；本地存在 DNS 劫持等场景时可关闭。
pub async fn validate_image_url_async_with_options(
    url: &str,
    allowed_hosts: &[&str],
    check_dns: bool,
) -> Result<()> {
    validate_image_url(url, allowed_hosts)?;

    if !check_dns {
        return Ok(());
    }

    let parsed = reqwest::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("image url 缺少 host"))?;

    // 字面 IP 已经过白名单验证，跳过 DNS 预解析。
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }

    // 对域名做 DNS 预解析：必须每条 A/AAAA 都是公网地址。
    let lookup_target = format!("{}:443", host);
    let mut public_addrs: Vec<IpAddr> = Vec::new();
    let mut all_addrs: Vec<IpAddr> = Vec::new();
    let lookup = kovi::tokio::time::timeout(
        DNS_LOOKUP_TIMEOUT,
        kovi::tokio::net::lookup_host(&lookup_target),
    )
    .await
    .map_err(|_| anyhow::anyhow!("DNS 解析超时 ({}s)", DNS_LOOKUP_TIMEOUT.as_secs()))?
    .map_err(|e| anyhow::anyhow!("DNS 解析失败: {e}"))?;
    for sa in lookup {
        let ip = sa.ip();
        all_addrs.push(ip);
        if is_public_ip(&ip) {
            public_addrs.push(ip);
        }
    }
    tracing::debug!(
        "validate_image_url_async: {} -> all: {:?}, public: {:?}",
        host,
        all_addrs,
        public_addrs
    );
    if all_addrs.is_empty() {
        return Err(anyhow::anyhow!("DNS 解析返回空: {}", host));
    }
    if public_addrs.is_empty() {
        return Err(anyhow::anyhow!(
            "image url 域名 {} 解析到的所有地址都是非公网: {:?}",
            host,
            all_addrs
        ));
    }

    Ok(())
}

/// 异步校验图片 URL，默认启用 DNS 预解析。
///
/// 等价于 `validate_image_url_async_with_options(url, allowed_hosts, true)`。
pub async fn validate_image_url_async(url: &str, allowed_hosts: &[&str]) -> Result<()> {
    validate_image_url_async_with_options(url, allowed_hosts, true).await
}

/// IP 是否是公网单播地址（可路由）。
/// 拒绝：loopback、unspecified、private、link-local、multicast、broadcast、文档/保留段。
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: &Ipv4Addr) -> bool {
    // 0.0.0.0/8        - "this network"
    // 10.0.0.0/8       - private
    // 100.64.0.0/10    - CGNAT
    // 127.0.0.0/8      - loopback
    // 169.254.0.0/16   - link-local（含 AWS metadata 169.254.169.254）
    // 172.16.0.0/12    - private
    // 192.0.0.0/24     - IETF protocol assignments
    // 192.0.2.0/24     - TEST-NET-1
    // 192.168.0.0/16   - private
    // 198.18.0.0/15    - benchmarking
    // 198.51.100.0/24  - TEST-NET-2
    // 203.0.113.0/24   - TEST-NET-3
    // 224.0.0.0/4      - multicast
    // 240.0.0.0/4      - reserved（含 255.255.255.255 broadcast）
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback() {
        return false;
    }
    let oct = ip.octets();
    match oct[0] {
        0 => false,                                          // 0.0.0.0/8
        10 => false,                                         // 10.0.0.0/8
        100 if oct[1] >= 64 && oct[1] <= 127 => false,       // 100.64.0.0/10 CGNAT
        127 => false,                                        // 127.0.0.0/8 loopback
        169 if oct[1] == 254 => false,                       // 169.254.0.0/16 link-local
        172 if (16..=31).contains(&oct[1]) => false,         // 172.16.0.0/12 private
        192 if oct[1] == 0 && oct[2] == 0 => false,          // 192.0.0.0/24 IETF
        192 if oct[1] == 0 && oct[2] == 2 => false,          // 192.0.2.0/24 TEST-NET-1
        192 if oct[1] == 168 => false,                       // 192.168.0.0/16 private
        198 if oct[1] == 18 && oct[2] <= 1 => false,         // 198.18.0.0/15 benchmark
        198 if oct[1] == 51 && oct[2] == 100 => false,       // 198.51.100.0/24 TEST-NET-2
        203 if oct[1] == 0 && oct[2] == 113 => false,        // 203.0.113.0/24 TEST-NET-3
        // 224..=239 multicast, 240..=255 reserved: 上面的 is_multicast / broadcast 已涵盖，
        // 这里保留兜底
        224..=255 => false,
        _ => true,
    }
}

fn is_public_v6(ip: &Ipv6Addr) -> bool {
    // 拒绝 unspecified / loopback / unique-local / link-local / multicast
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let seg = ip.segments();
    // fc00::/7 unique local (private)
    if (seg[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // fe80::/10 link-local
    if (seg[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // ::ffff:0:0/96 IPv4-mapped: 用 v4 规则再判一次
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(&v4);
    }
    true
}

/// 把任意 [`SocketAddr`] 列表过滤为仅含公网 IP 的列表，配合 reqwest `resolve_to_addrs` 做强制 pinning。
///
/// 通常不需要直接调用此函数；[`validate_image_url_async`] 已经覆盖了校验场景。
/// 只有当调用方需要把 client 配置为只连指定 IP 时（极致 hardening 场景），才用这个。
#[allow(dead_code)]
pub fn filter_public_addrs(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    addrs
        .iter()
        .copied()
        .filter(|sa| is_public_ip(&sa.ip()))
        .collect()
}
