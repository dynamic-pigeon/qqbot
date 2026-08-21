//! 共享的 SSRF 防御：图片 URL 必须使用 https 且 host 位于白名单。
//!
//! 设计要点：
//! - 基础校验始终检查 scheme 和 host 白名单。
//! - 开启私网保护后，字面 IP 和域名解析结果都必须是公网单播地址，域名连接会固定到
//!   本次校验通过的地址，防止 DNS rebinding 绕过 URL 字符串层校验。
//! - 私网保护默认关闭，以兼容 DNS 或流量被转发到本地地址的部署环境。
//!
//! 调用方约定：
//! - 调用 `validate_image_url` 前必须有合法 `allowed_hosts` 列表；运行时不应接收 user 输入的 host。
//! - 调 HTTP 客户端的 `redirect` 策略保持 `Policy::none()`，否则 redirect 到的地址
//!   仍会绕过白名单（与 DNS 预解析无关，是另一道墙）。

use std::{
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::LazyLock,
    time::Duration,
};

use anyhow::Result;

const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
pub const PRIVATE_NETWORK_PROTECTION_ENV: &str = "PRIVATE_NETWORK_PROTECTION";

/// 全局共享的 HTTP client，复用连接池，避免每次下载都重新 TCP/TLS 握手。
/// 仅在未开启私网保护时使用；开启后每次请求需要按本次校验结果做 DNS pinning，
/// 必须构建带 `resolve_to_addrs` 的临时 client。
static SHARED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("构建共享 reqwest client 失败")
});

static PRIVATE_NETWORK_PROTECTION: LazyLock<bool> = LazyLock::new(|| {
    if let Ok(value) = env::var(PRIVATE_NETWORK_PROTECTION_ENV) {
        match parse_env_bool(&value) {
            Some(enabled) => return enabled,
            None => tracing::warn!(
                "{PRIVATE_NETWORK_PROTECTION_ENV}={value:?} 无效，回退到 config.toml；可用值: true/false, 1/0, yes/no, on/off"
            ),
        }
    }
    crate::config::value()
        .get("network")
        .and_then(|v| v.get("private_network_protection"))
        .and_then(crate::config::Value::as_bool)
        .unwrap_or(false)
});

fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// 是否启用私网保护。
///
/// 优先级：环境变量 `PRIVATE_NETWORK_PROTECTION` > 根目录 `config.toml` > 默认关闭。
pub fn private_network_protection_enabled() -> bool {
    *PRIVATE_NETWORK_PROTECTION
}

/// 校验图片 URL：仅允许 https + 白名单 host；启用私网保护时，字面 IP 还必须是公网单播地址。
pub fn validate_image_url(url: &str, allowed_hosts: &[&str]) -> Result<()> {
    validate_image_url_with_options(url, allowed_hosts, private_network_protection_enabled())
}

/// 使用指定的私网保护策略校验图片 URL。
///
/// `protect_private_network` 启用时，禁止 loopback、private、link-local 等非公网地址。
pub fn validate_image_url_with_options(
    url: &str,
    allowed_hosts: &[&str],
    protect_private_network: bool,
) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("image url 解析失败: {e}"))?;

    if parsed.scheme() != "https" {
        return Err(anyhow::anyhow!(
            "image url scheme 必须是 https，收到: {}",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("image url 缺少 host"))?;

    let host_lower = host.to_ascii_lowercase();
    let allowed = allowed_hosts.iter().any(|allowed| {
        let allowed = allowed.to_ascii_lowercase();
        host_lower == allowed || host_lower.ends_with(&format!(".{allowed}"))
    });
    if !allowed {
        return Err(anyhow::anyhow!("image url host 不在白名单: {}", host));
    }

    // 开启私网保护后，即使字面 IP 已列入白名单，也只允许公网单播地址。
    if protect_private_network
        && let Some(ip) = literal_ip_from_url(&parsed)
        && !is_public_ip(&ip)
    {
        return Err(anyhow::anyhow!("image url host 命中非公网地址: {}", ip));
    }

    Ok(())
}

/// URL 的 host 是否是字面 IP。IPv6 字面 host（`host_str()` 返回值带方括号）也会被识别。
fn literal_ip_from_url(parsed: &reqwest::Url) -> Option<IpAddr> {
    let host = parsed.host_str()?;
    let stripped = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    stripped.parse::<IpAddr>().ok()
}

/// 异步校验图片 URL：在基础 URL 校验上可选对域名做 DNS 预解析，
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
/// `protect_private_network` 控制是否拒绝非公网地址并执行 DNS pinning。
pub async fn validate_image_url_async_with_options(
    url: &str,
    allowed_hosts: &[&str],
    protect_private_network: bool,
) -> Result<()> {
    validate_image_url_with_options(url, allowed_hosts, protect_private_network)?;

    if !protect_private_network {
        return Ok(());
    }

    resolve_public_addrs(url).await?;

    Ok(())
}

/// 下载经过白名单校验的图片，并对实际响应体实施硬字节上限。
///
/// 默认复用全局共享 client 以命中连接池；启用私网保护时改用带 DNS pinning 的
/// 临时 client，固定使用本次校验通过的地址，避免校验与连接之间再次解析域名造成
/// DNS rebinding。
pub async fn download_image_limited(
    url: &str,
    allowed_hosts: &[&str],
    max_bytes: usize,
    request_timeout: Duration,
) -> Result<Vec<u8>> {
    let protect_private_network = private_network_protection_enabled();
    validate_image_url_with_options(url, allowed_hosts, protect_private_network)?;
    if max_bytes == 0 {
        return Err(anyhow::anyhow!("图片大小上限必须大于 0"));
    }

    let parsed = reqwest::Url::parse(url)?;

    let response = if protect_private_network {
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("image url 缺少 host"))?
            .to_string();
        let addrs = resolve_public_addrs(url).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&host, &addrs)
            .build()?;
        client.get(parsed).timeout(request_timeout).send().await?
    } else {
        SHARED_CLIENT
            .get(parsed)
            .timeout(request_timeout)
            .send()
            .await?
    };
    let response = response.error_for_status()?;
    if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) {
        let content_type = content_type.to_str().unwrap_or_default();
        if !content_type.to_ascii_lowercase().starts_with("image/") {
            return Err(anyhow::anyhow!("响应不是图片: {content_type}"));
        }
    }

    read_response_limited(response, max_bytes).await
}

/// 流式读取 HTTP 响应，并在响应体超过 `max_bytes` 时立即中止。
pub async fn read_response_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    response.error_for_status_ref()?;
    if max_bytes == 0 {
        return Err(anyhow::anyhow!("响应大小上限必须大于 0"));
    }
    if let Some(length) = response.content_length()
        && length > max_bytes as u64
    {
        return Err(anyhow::anyhow!(
            "响应超过大小上限: {length} > {max_bytes} bytes"
        ));
    }

    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(max_bytes);
    let mut body = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await? {
        append_limited(&mut body, &chunk, max_bytes)?;
    }
    Ok(body)
}

fn append_limited(body: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> Result<()> {
    let next_len = body
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| anyhow::anyhow!("响应大小溢出"))?;
    if next_len > max_bytes {
        return Err(anyhow::anyhow!("响应超过大小上限: > {max_bytes} bytes"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

async fn resolve_public_addrs(url: &str) -> Result<Vec<SocketAddr>> {
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("image url 缺少 host"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("image url 缺少端口"))?;

    if let Some(ip) = literal_ip_from_url(&parsed) {
        if !is_public_ip(&ip) {
            return Err(anyhow::anyhow!("image url host 命中非公网地址: {ip}"));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let lookup = kovi::tokio::time::timeout(
        DNS_LOOKUP_TIMEOUT,
        kovi::tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("DNS 解析超时 ({}s)", DNS_LOOKUP_TIMEOUT.as_secs()))?
    .map_err(|e| anyhow::anyhow!("DNS 解析失败: {e}"))?;
    let addrs: Vec<SocketAddr> = lookup.collect();
    if addrs.is_empty() {
        return Err(anyhow::anyhow!("DNS 解析返回空: {host}"));
    }
    if let Some(private) = addrs.iter().find(|addr| !is_public_ip(&addr.ip())) {
        return Err(anyhow::anyhow!(
            "image url 域名 {host} 包含非公网解析地址: {}",
            private.ip()
        ));
    }
    tracing::debug!("validated and pinned image host {host} to {addrs:?}");
    Ok(addrs)
}

/// 异步校验图片 URL，私网保护策略由 `private_network_protection_enabled` 决定。
///
/// 未设置环境变量且 `config.toml` 未开启时，关闭私网地址校验和 DNS pinning。
pub async fn validate_image_url_async(url: &str, allowed_hosts: &[&str]) -> Result<()> {
    validate_image_url_async_with_options(url, allowed_hosts, private_network_protection_enabled())
        .await
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
        0 => false,                                    // 0.0.0.0/8
        10 => false,                                   // 10.0.0.0/8
        100 if oct[1] >= 64 && oct[1] <= 127 => false, // 100.64.0.0/10 CGNAT
        127 => false,                                  // 127.0.0.0/8 loopback
        169 if oct[1] == 254 => false,                 // 169.254.0.0/16 link-local
        172 if (16..=31).contains(&oct[1]) => false,   // 172.16.0.0/12 private
        192 if oct[1] == 0 && oct[2] == 0 => false,    // 192.0.0.0/24 IETF
        192 if oct[1] == 0 && oct[2] == 2 => false,    // 192.0.2.0/24 TEST-NET-1
        192 if oct[1] == 168 => false,                 // 192.168.0.0/16 private
        198 if oct[1] == 18 && oct[2] <= 1 => false,   // 198.18.0.0/15 benchmark
        198 if oct[1] == 51 && oct[2] == 100 => false, // 198.51.100.0/24 TEST-NET-2
        203 if oct[1] == 0 && oct[2] == 113 => false,  // 203.0.113.0/24 TEST-NET-3
        // 224..=239 multicast, 240..=255 reserved: 上面的 is_multicast / broadcast 已涵盖，
        // 这里保留兜底
        224..=255 => false,
        _ => true,
    }
}

fn is_public_v6(ip: &Ipv6Addr) -> bool {
    // 只接受 2000::/3 全球单播，并额外拒绝其中的文档和特殊用途网段。
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
    if (seg[0] & 0xe000) != 0x2000 {
        return false;
    }
    // 2001:db8::/32 documentation
    if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        return false;
    }
    // 2001:2::/48 benchmarking
    if seg[0] == 0x2001 && seg[1] == 0x0002 && seg[2] == 0 {
        return false;
    }
    // 3fff::/20 documentation
    if seg[0] == 0x3fff && (seg[1] & 0xf000) == 0 {
        return false;
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

#[cfg(test)]
mod tests {
    use super::{append_limited, parse_env_bool};

    #[test]
    fn parses_private_network_protection_values() {
        assert_eq!(parse_env_bool("true"), Some(true));
        assert_eq!(parse_env_bool(" ON "), Some(true));
        assert_eq!(parse_env_bool("0"), Some(false));
        assert_eq!(parse_env_bool("no"), Some(false));
        assert_eq!(parse_env_bool(""), None);
        assert_eq!(parse_env_bool("invalid"), None);
    }

    #[test]
    fn chunked_body_cannot_exceed_limit() {
        let mut body = Vec::new();
        append_limited(&mut body, b"hello", 8).unwrap();
        assert!(append_limited(&mut body, b" world", 8).is_err());
        assert_eq!(body, b"hello");
    }

    #[test]
    fn chunks_up_to_exact_limit_are_accepted() {
        let mut body = Vec::new();
        append_limited(&mut body, b"hel", 5).unwrap();
        append_limited(&mut body, b"lo", 5).unwrap();
        assert_eq!(body, b"hello");
    }
}
