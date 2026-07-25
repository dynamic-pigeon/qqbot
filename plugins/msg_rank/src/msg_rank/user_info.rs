use std::time::{Duration, Instant};

use anyhow::Result;
use kovi::{RuntimeBot, serde_json, tokio};
use kovi_onebot::OnebotTrait as _;
use moka::future::Cache;

#[derive(Clone)]
pub(crate) struct UserInfo {
    #[allow(dead_code)]
    pub user_id: i64,
    pub nickname: String,
    pub avatar: bytes::Bytes,
    /// 条目被缓存的时间，用于区分 fresh / stale。
    pub fetched_at: Instant,
}

#[derive(serde::Deserialize)]
struct MemberInfoApiResponse {
    card: String,
    nickname: String,
    user_id: i64,
}

// 按字节限容：头像（最大 2MB）是条目体积的大头，只按条数限制时缓存可能涨到 GB 级。
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

static USER_INFO_CACHE: std::sync::LazyLock<Cache<(i64, i64), UserInfo>> =
    std::sync::LazyLock::new(|| {
        Cache::builder()
            .weigher(|_key: &(i64, i64), info: &UserInfo| {
                (info.avatar.len() + info.nickname.len() + 64) as u32
            })
            .max_capacity(MAX_CACHE_BYTES)
            // TTL 24 小时只是条目存活上限；是否刷新由 fetched_at 控制。
            .time_to_live(Duration::from_secs(60 * 60 * 24))
            .build()
    });

/// 1 小时内视为 fresh，超过则重新拉取，失败时仍可回落到 stale 缓存。
const FRESH_DURATION: Duration = Duration::from_secs(60 * 60);

/// 含初始请求最多 3 次尝试：初始、200ms 后、500ms 后。
const MAX_ATTEMPTS: usize = 3;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(200), Duration::from_millis(500)];

fn retry_delay(attempt: usize) -> Duration {
    RETRY_DELAYS
        .get(attempt.saturating_sub(1))
        .copied()
        .unwrap_or(RETRY_DELAYS[RETRY_DELAYS.len() - 1])
}

/// 通过 bot API 获取单个群成员的 UserInfo（带缓存、stale fallback 与重试）。
pub(super) async fn get_user_info(
    bot: &RuntimeBot,
    group_id: i64,
    user_id: i64,
) -> Result<UserInfo> {
    let key = (group_id, user_id);

    // fresh 缓存直接返回，不走 API。
    let stale = USER_INFO_CACHE.get(&key).await;
    if let Some(info) = &stale
        && info.fetched_at.elapsed() < FRESH_DURATION
    {
        return Ok(info.clone());
    }

    // try_get_with 对 TTL 内已存在的条目直接返回、不会执行初始化闭包，
    // 所以 stale 条目必须先移除，刷新才会真正发生。
    if stale.is_some() {
        USER_INFO_CACHE.invalidate(&key).await;
    }

    let bot = bot.clone();
    match USER_INFO_CACHE
        .try_get_with(key, async move {
            fetch_user_info(&bot, group_id, user_id)
                .await
                .map_err(|e| e.to_string())
        })
        .await
    {
        Ok(info) => Ok(info),
        Err(arc) => {
            // 刷新失败时回落到刷新前保存的 stale 条目。
            if let Some(stale) = stale {
                tracing::warn!("获取用户 {} 信息失败，使用缓存数据: {}", user_id, arc);
                Ok(stale)
            } else {
                Err(anyhow::anyhow!("{arc}"))
            }
        }
    }
}

async fn fetch_user_info(bot: &RuntimeBot, group_id: i64, user_id: i64) -> Result<UserInfo> {
    let (member_result, avatar) =
        tokio::join!(fetch_member_with_retry(bot, group_id, user_id), async {
            fetch_avatar_with_retry(user_id).await.unwrap_or_else(|e| {
                tracing::warn!("获取头像失败，user_id={}: {}", user_id, e);
                bytes::Bytes::new()
            })
        });

    let (uid, nickname) = member_result?;
    Ok(UserInfo {
        user_id: uid,
        nickname,
        avatar,
        fetched_at: Instant::now(),
    })
}

async fn fetch_member_with_retry(
    bot: &RuntimeBot,
    group_id: i64,
    user_id: i64,
) -> Result<(i64, String)> {
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        match bot.get_group_member_info(group_id, user_id, false).await {
            Ok(resp) => match parse_member_info(resp).await {
                Ok(info) => return Ok(info),
                Err(e) => {
                    tracing::warn!("解析群成员信息失败 (attempt {}): {}", attempt + 1, e);
                    last_err = Some(e);
                }
            },
            Err(e) => {
                tracing::error!(
                    "调用 API get_group_member_info 失败 (attempt {}): {}",
                    attempt + 1,
                    e
                );
                last_err = Some(anyhow::anyhow!("获取群成员信息失败"));
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

async fn fetch_avatar_with_retry(user_id: i64) -> Result<bytes::Bytes> {
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        let avatar_url = format!("https://q4.qlogo.cn/headimg_dl?dst_uin={user_id}&spec=640");
        match utils::download_image_limited(
            &avatar_url,
            &["qlogo.cn"],
            2 * 1024 * 1024,
            Duration::from_secs(10),
        )
        .await
        {
            Ok(bytes) if !bytes.is_empty() => return Ok(bytes::Bytes::from(bytes)),
            Ok(_) => return Err(anyhow::anyhow!("头像为空")),
            Err(e) => last_err = Some(e),
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
            fetched_at: Instant::now(),
        };
        USER_INFO_CACHE.insert((1, 123456), info.clone()).await;

        // 直接查缓存，不依赖 bot API
        let cached = USER_INFO_CACHE.get(&(1, 123456)).await.unwrap();
        assert_eq!(cached.user_id, 123456);
        assert_eq!(cached.nickname, "test");
        assert_eq!(cached.avatar.as_ref(), b"avatar");
    }
}
