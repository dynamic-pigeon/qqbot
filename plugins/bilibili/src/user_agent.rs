//! 插件级 User-Agent：`BILIBILI_USER_AGENT` 环境变量优先，未设置或空白时
//! 回退内置默认值。共享 CLIENT（BV 解析等）、动态 HTTP 直连与 Chromium
//! 后备共用同一份，避免各路径指纹不一致。

use std::sync::LazyLock;

/// 默认 User-Agent，用于 `BILIBILI_USER_AGENT` 未设置时的 fallback。
/// 选用 Chrome Linux 最新稳定版的常见格式。
pub(crate) const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";

/// 从 `BILIBILI_USER_AGENT` 环境变量解析 UA；未设置或空白时回退到默认。
fn resolve_user_agent(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string())
}

static USER_AGENT: LazyLock<String> =
    LazyLock::new(|| resolve_user_agent(std::env::var("BILIBILI_USER_AGENT").ok().as_deref()));

pub(crate) fn user_agent() -> &'static str {
    USER_AGENT.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_trims_and_falls_back() {
        assert_eq!(resolve_user_agent(None), DEFAULT_USER_AGENT);
        assert_eq!(resolve_user_agent(Some("")), DEFAULT_USER_AGENT);
        assert_eq!(resolve_user_agent(Some("  \t")), DEFAULT_USER_AGENT);
        assert_eq!(
            resolve_user_agent(Some("  CustomBot/1.0  ")),
            "CustomBot/1.0"
        );
    }
}
