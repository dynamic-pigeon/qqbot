//! 群成员信息缓存。
//!
//! 官方 API 没有按需查询群成员的接口，成员的昵称 / 群名片 / 头像只在
//! 消息事件里随发送者一起下发。事件流经过时顺手更新缓存，
//! `official_member_info` 请求全部由缓存应答。

use std::collections::HashMap;

use serde_json::json;

/// member_openid 是群维度的，所以键是 (群 openid, 成员 openid) 二元组。
pub struct MemberCache {
    inner: std::sync::Mutex<HashMap<(String, String), Member>>,
}

#[derive(Clone)]
pub struct Member {
    pub nickname: String,
    pub card: Option<String>,
    pub avatar_url: Option<String>,
}

impl MemberCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn update(
        &self,
        group_openid: &str,
        member_openid: &str,
        nickname: &str,
        card: Option<&str>,
        avatar_url: Option<&str>,
    ) {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        cache.insert(
            (group_openid.to_owned(), member_openid.to_owned()),
            Member {
                nickname: nickname.to_owned(),
                card: card.map(str::to_owned),
                avatar_url: avatar_url.map(str::to_owned),
            },
        );
    }

    /// 昵称 / 群名片 / 头像，缓存未命中返回 None。
    pub fn member_info(
        &self,
        group_openid: &str,
        member_openid: &str,
    ) -> Option<serde_json::Value> {
        let cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let member = cache.get(&(group_openid.to_owned(), member_openid.to_owned()))?;
        Some(json!({
            "nickname": member.nickname,
            "card": member.card,
            "avatar": member.avatar_url,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 事件更新后可查询() {
        let cache = MemberCache::new();
        cache.update("G1", "U1", "昵称", Some("群名片"), Some("https://avatar"));
        let info = cache.member_info("G1", "U1").unwrap();
        assert_eq!(info["nickname"], "昵称");
        assert_eq!(info["card"], "群名片");
        assert_eq!(info["avatar"], "https://avatar");
    }

    #[test]
    fn 未命中无成员信息() {
        let cache = MemberCache::new();
        assert!(cache.member_info("G1", "U2").is_none());
    }
}
