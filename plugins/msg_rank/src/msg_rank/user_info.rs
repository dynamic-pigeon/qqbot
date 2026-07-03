use std::time::Duration;

use anyhow::Result;
use kovi::{RuntimeBot, serde_json, tokio};
use kovi_onebot::OnebotTrait as _;
use moka::future::Cache;

use crate::HTTP_CLIENT;

#[derive(Clone)]
pub(crate) struct UserInfo {
    #[allow(dead_code)]
    pub user_id: i64,
    pub nickname: String,
    pub avatar: bytes::Bytes,
}

#[derive(serde::Deserialize)]
struct MemberInfoApiResponse {
    card: String,
    nickname: String,
    user_id: i64,
}

static USER_INFO_CACHE: std::sync::LazyLock<Cache<(i64, i64), UserInfo>> =
    std::sync::LazyLock::new(|| {
        Cache::builder()
            .max_capacity(2048)
            .time_to_live(Duration::from_secs(60 * 60))
            .build()
    });

const MAX_RETRIES: usize = 2;

/// 通过 bot API 获取单个群成员的 UserInfo（带缓存与重试）。
pub(super) async fn get_user_info(
    bot: &RuntimeBot,
    group_id: i64,
    user_id: i64,
) -> Result<UserInfo> {
    let key = (group_id, user_id);
    if let Some(cached) = USER_INFO_CACHE.get(&key).await {
        return Ok(cached);
    }

    match fetch_user_info(bot, group_id, user_id).await {
        Ok(info) => {
            USER_INFO_CACHE.insert(key, info.clone()).await;
            Ok(info)
        }
        Err(e) => {
            tracing::warn!("获取用户 {} 信息失败: {}", user_id, e);
            Err(e)
        }
    }
}

async fn fetch_user_info(bot: &RuntimeBot, group_id: i64, user_id: i64) -> Result<UserInfo> {
    let mut last_err = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = match attempt {
                1 => Duration::from_millis(200),
                _ => Duration::from_millis(500),
            };
            tokio::time::sleep(delay).await;
        }

        let api_fut = async {
            let resp = bot
                .get_group_member_info(group_id, user_id, false)
                .await
                .map_err(|e| {
                    tracing::error!("调用 API get_group_member_info 失败: {}", e);
                    anyhow::anyhow!("获取群成员信息失败")
                })?;
            parse_member_info(resp).await
        };
        let avatar_fut = get_avatar(user_id);

        let (member, avatar) = tokio::join!(api_fut, avatar_fut);

        match member {
            Ok((uid, nickname)) => {
                let avatar = avatar.unwrap_or_else(|e| {
                    tracing::warn!("获取头像失败，user_id={}: {}", user_id, e);
                    bytes::Bytes::new()
                });
                return Ok(UserInfo {
                    user_id: uid,
                    nickname,
                    avatar,
                });
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("获取用户 {} 信息失败", user_id)))
}

async fn parse_member_info(resp: kovi::ApiReturn) -> Result<(i64, String)> {
    if resp.status != "ok" {
        anyhow::bail!("API请求失败: {:?}", resp);
    }

    let item = serde_json::from_value::<MemberInfoApiResponse>(resp.data)?;
    let nickname = if item.card.is_empty() {
        item.nickname
    } else {
        item.card
    };

    Ok((item.user_id, nickname))
}

async fn get_avatar(user_id: i64) -> Result<bytes::Bytes> {
    let mut last_err = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = match attempt {
                1 => Duration::from_millis(200),
                _ => Duration::from_millis(500),
            };
            tokio::time::sleep(delay).await;
        }

        let avatar_url = format!("https://q4.qlogo.cn/headimg_dl?dst_uin={user_id}&spec=640");
        match HTTP_CLIENT.get(&avatar_url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
                    Ok(_) => last_err = Some(anyhow::anyhow!("头像为空")),
                    Err(e) => last_err = Some(e.into()),
                },
                Err(e) => last_err = Some(e.into()),
            },
            Err(e) => last_err = Some(e.into()),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("获取头像失败: user_id={}", user_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_user_info_cache_stores_and_returns_value() {
        let info = UserInfo {
            user_id: 123456,
            nickname: "test".to_string(),
            avatar: bytes::Bytes::from_static(b"avatar"),
        };
        USER_INFO_CACHE.insert((1, 123456), info.clone()).await;

        // 直接查缓存，不依赖 bot API
        let cached = USER_INFO_CACHE.get(&(1, 123456)).await.unwrap();
        assert_eq!(cached.user_id, 123456);
        assert_eq!(cached.nickname, "test");
        assert_eq!(cached.avatar.as_ref(), b"avatar");
    }
}
