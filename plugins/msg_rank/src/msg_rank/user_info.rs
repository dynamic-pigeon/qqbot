use std::time::{Duration, Instant};

use anyhow::Result;
use kovi::{RuntimeBot, tokio};
use moka::future::Cache;

#[derive(Clone)]
pub(crate) struct UserInfo {
    #[allow(dead_code)]
    pub user_key: String,
    pub nickname: String,
    pub avatar: bytes::Bytes,
    /// 条目被缓存的时间，用于区分 fresh / stale。
    pub fetched_at: Instant,
}

/// 拉取失败时的兜底条目：身份键当昵称、空头像。
pub(super) fn fallback_user_info(user_key: &str) -> UserInfo {
    UserInfo {
        user_key: user_key.to_owned(),
        nickname: user_key.to_owned(),
        avatar: bytes::Bytes::new(),
        fetched_at: Instant::now(),
    }
}

// ── 成员资料缓存（L2，msg.db 的 MEMBER_PROFILE 表）───────────────────────
//
// 内存缓存重启即空；官方栈成员信息只能来自事件流、头像 URL 又有时效，
// 重启后没再发言的成员昵称会退化成 openid 串。昵称与头像进 SQLite：
// 采集器路径发言即写（头像 URL 不变不重下载字节），渲染路径查不到
// 成员时整行兜底读。

/// 资料保留窗口；榜上成员必然活跃，窗口内被刷新过的行不会被清扫。
const PROFILE_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

/// 资料刷新窗口：窗口内到达的消息不触发任何库写与下载。
const PROFILE_REFRESH_SECS: i64 = 24 * 60 * 60;

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// 采集器路径的资料维护：库内资料仍在刷新窗口内就不动；过期或首见才
/// 重新下载头像整行写入。下载失败时至少把昵称行写起来（同样计入窗口，
/// 下个窗口再试头像），避免每条消息都重试下载。
pub(crate) async fn observe(
    group: &str,
    user: &str,
    nickname: &str,
    card: Option<&str>,
    avatar_url: Option<&str>,
) {
    let Ok(pool) = crate::db::get_pool() else {
        return;
    };
    if profile_is_fresh(pool, group, user).await {
        return;
    }
    let shown = match card {
        Some(card) if !card.is_empty() => card.to_owned(),
        _ => nickname.to_owned(),
    };

    let avatar = match avatar_url {
        Some(url) => match fetch_avatar_with_retry(url).await {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!("头像下载失败 user={user}: {e:#}");
                None
            }
        },
        None => None,
    };
    let result = match avatar.as_ref() {
        Some(bytes) => upsert_profile(pool, group, user, &shown, Some(bytes)).await,
        None => touch_profile(pool, group, user, &shown).await,
    };
    if let Err(e) = result {
        tracing::warn!("成员资料落库失败 user={user}: {e:#}");
    }
}

/// 库内资料是否仍在刷新窗口内。官方栈头像 URL 每条消息都变（带签名
/// 的时效链接），不能当变更依据，统一用时间窗淘汰。
async fn profile_is_fresh(pool: &sqlx::SqlitePool, group: &str, user: &str) -> bool {
    sqlx::query_as::<_, (i64,)>(
        "SELECT saved_at FROM MEMBER_PROFILE WHERE group_id = ? AND user_id = ?",
    )
    .bind(group)
    .bind(user)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some_and(|(saved_at,)| saved_at >= epoch_now() - PROFILE_REFRESH_SECS)
}

/// 整行写入。avatar 为 None 时保留库内旧字节（COALESCE）。
async fn upsert_profile(
    pool: &sqlx::SqlitePool,
    group: &str,
    user: &str,
    nickname: &str,
    avatar: Option<&[u8]>,
) -> Result<()> {
    sqlx::query(
        "
INSERT INTO MEMBER_PROFILE (group_id, user_id, nickname, avatar, saved_at)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT (group_id, user_id) DO UPDATE SET
    nickname = excluded.nickname,
    avatar = COALESCE(excluded.avatar, MEMBER_PROFILE.avatar),
    saved_at = excluded.saved_at
",
    )
    .bind(group)
    .bind(user)
    .bind(nickname)
    .bind(avatar)
    .bind(epoch_now())
    .execute(pool)
    .await?;
    Ok(())
}

/// 只刷新昵称与活跃时间，不动头像列（下载失败时的降级写；
/// 首见即失败也把昵称行建起来，头像留给下个窗口补）。
async fn touch_profile(
    pool: &sqlx::SqlitePool,
    group: &str,
    user: &str,
    nickname: &str,
) -> Result<()> {
    sqlx::query(
        "
INSERT INTO MEMBER_PROFILE (group_id, user_id, nickname, avatar, saved_at)
VALUES (?, ?, ?, NULL, ?)
ON CONFLICT (group_id, user_id) DO UPDATE SET
    nickname = excluded.nickname,
    saved_at = excluded.saved_at
",
    )
    .bind(group)
    .bind(user)
    .bind(nickname)
    .bind(epoch_now())
    .execute(pool)
    .await?;
    Ok(())
}

/// 成员信息查不到时整行恢复（昵称 + 头像，容忍过期内容）。
async fn load_profile(pool: &sqlx::SqlitePool, group: &str, user: &str) -> Option<UserInfo> {
    let (nickname, avatar): (String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT nickname, avatar FROM MEMBER_PROFILE WHERE group_id = ? AND user_id = ?",
    )
    .bind(group)
    .bind(user)
    .fetch_optional(pool)
    .await
    .ok()??;
    Some(UserInfo {
        user_key: user.to_owned(),
        nickname,
        avatar: avatar.map(bytes::Bytes::from).unwrap_or_default(),
        fetched_at: Instant::now(),
    })
}

/// 只取库内的头像字节，头像下载失败时兜底。
async fn db_avatar(group: &str, user: &str) -> bytes::Bytes {
    let Ok(pool) = crate::db::get_pool() else {
        return bytes::Bytes::new();
    };
    sqlx::query_as::<_, (Option<Vec<u8>>,)>(
        "SELECT avatar FROM MEMBER_PROFILE WHERE group_id = ? AND user_id = ?",
    )
    .bind(group)
    .bind(user)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|(avatar,)| avatar)
    .map(bytes::Bytes::from)
    .unwrap_or_default()
}

/// 清扫落盘时间超窗的资料行，挂在 db.rs 的每日 cleanup tick。
pub(crate) async fn delete_expired_profiles(pool: &sqlx::SqlitePool) -> Result<()> {
    let cutoff = epoch_now() - PROFILE_RETENTION_SECS;
    let result = sqlx::query("DELETE FROM MEMBER_PROFILE WHERE saved_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        tracing::info!("成员资料清扫: 删除 {} 行", result.rows_affected());
    }
    Ok(())
}

// 按字节限容：头像（最大 2MB）是条目体积的大头，只按条数限制时缓存可能涨到 GB 级。
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

static USER_INFO_CACHE: std::sync::LazyLock<Cache<(String, String), UserInfo>> =
    std::sync::LazyLock::new(|| {
        Cache::builder()
            .weigher(|_key: &(String, String), info: &UserInfo| {
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

/// 获取单个群成员的 UserInfo（带缓存、stale fallback 与重试）。
/// 身份键：官方栈 openid，OneBot 栈 QQ 号十进制串。
pub(super) async fn get_user_info(
    bot: &RuntimeBot,
    group_key: &str,
    user_key: &str,
) -> Result<UserInfo> {
    let key = (group_key.to_owned(), user_key.to_owned());

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
            fetch_user_info(&bot, group_key, user_key)
                .await
                .map_err(|e| e.to_string())
        })
        .await
    {
        Ok(info) => Ok(info),
        Err(arc) => {
            // 刷新失败时回落到刷新前保存的 stale 条目。
            if let Some(stale) = stale {
                tracing::warn!("获取用户 {} 信息失败，使用缓存数据: {}", user_key, arc);
                Ok(stale)
            } else {
                Err(anyhow::anyhow!("{arc}"))
            }
        }
    }
}

async fn fetch_user_info(bot: &RuntimeBot, group_key: &str, user_key: &str) -> Result<UserInfo> {
    let profile = match fetch_member_with_retry(bot, group_key, user_key).await {
        Ok(profile) => profile,
        Err(e) => {
            // 成员信息不可用（官方栈重启后该成员未再发言）：库内资料兜底。
            if let Ok(pool) = crate::db::get_pool()
                && let Some(cached) = load_profile(pool, group_key, user_key).await
            {
                tracing::warn!("成员信息不可用，使用库内资料 user={user_key}: {e:#}");
                return Ok(cached);
            }
            return Err(e);
        }
    };

    let avatar_url = profile.avatar.clone();
    let avatar = match avatar_url.as_deref() {
        Some(url) => match fetch_avatar_with_retry(url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("获取头像失败，user={}: {e:#}", user_key);
                db_avatar(group_key, user_key).await
            }
        },
        None => bytes::Bytes::new(),
    };

    let nickname = match profile.card.as_deref() {
        Some(card) if !card.is_empty() => card.to_owned(),
        _ => profile.nickname,
    };
    if let Ok(pool) = crate::db::get_pool()
        && let Err(e) =
            upsert_profile(pool, group_key, user_key, &nickname, Some(avatar.as_ref())).await
    {
        tracing::warn!("成员资料落库失败 user={user_key}: {e:#}");
    }
    Ok(UserInfo {
        user_key: user_key.to_owned(),
        nickname,
        avatar,
        fetched_at: Instant::now(),
    })
}

/// 成员展示信息走门面：官方栈来自事件流缓存，OneBot 栈查协议端。
async fn fetch_member_with_retry(
    bot: &RuntimeBot,
    group_key: &str,
    user_key: &str,
) -> Result<utils::bot::MemberProfile> {
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        match utils::bot::member_profile(bot, group_key, user_key).await {
            Ok(profile) if !profile.nickname.is_empty() => return Ok(profile),
            Ok(_) => last_err = Some(anyhow::anyhow!("成员信息为空（官方栈需先见到该成员发言）")),
            Err(e) => {
                tracing::warn!("获取群成员信息失败 (attempt {}): {}", attempt + 1, e);
                last_err = Some(anyhow::anyhow!("{e}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("获取用户 {user_key} 信息失败")))
}

/// 头像 URL：官方栈是事件下发的 multimedia 直链，OneBot 栈按 QQ 号拼 qlogo。
async fn fetch_avatar_with_retry(avatar_url: &str) -> Result<bytes::Bytes> {
    let mut last_err = None;

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        match utils::download_image_limited(
            avatar_url,
            &["qlogo.cn", "qq.com.cn"],
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

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("获取头像失败: {avatar_url}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::ensure_member_profile_table(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn 落库昵称与头像后可整条恢复() {
        let pool = test_pool().await;
        upsert_profile(&pool, "G", "U", "昵称", Some(b"avatar-bytes"))
            .await
            .unwrap();
        let info = load_profile(&pool, "G", "U").await.unwrap();
        assert_eq!(info.nickname, "昵称");
        assert_eq!(&*info.avatar, b"avatar-bytes");
    }

    #[tokio::test]
    async fn 刷新窗口内视为新鲜() {
        let pool = test_pool().await;
        assert!(!profile_is_fresh(&pool, "G", "U").await);
        touch_profile(&pool, "G", "U", "名字").await.unwrap();
        assert!(profile_is_fresh(&pool, "G", "U").await);
        // 落库时间挪到窗口之外即过期，采集路径会重新下载整行。
        sqlx::query(
            "UPDATE MEMBER_PROFILE SET saved_at = ? WHERE group_id = 'G' AND user_id = 'U'",
        )
        .bind(epoch_now() - PROFILE_REFRESH_SECS - 1)
        .execute(&pool)
        .await
        .unwrap();
        assert!(!profile_is_fresh(&pool, "G", "U").await);
    }

    #[tokio::test]
    async fn touch保留旧头像() {
        let pool = test_pool().await;
        upsert_profile(&pool, "G", "U", "旧名", Some(b"avatar"))
            .await
            .unwrap();
        touch_profile(&pool, "G", "U", "新名").await.unwrap();
        let info = load_profile(&pool, "G", "U").await.unwrap();
        assert_eq!(info.nickname, "新名");
        assert_eq!(&*info.avatar, b"avatar");
    }

    #[tokio::test]
    async fn touch新建行不丢昵称() {
        let pool = test_pool().await;
        touch_profile(&pool, "G", "U", "名字").await.unwrap();
        let info = load_profile(&pool, "G", "U").await.unwrap();
        assert_eq!(info.nickname, "名字");
        assert!(info.avatar.is_empty());
    }

    #[tokio::test]
    async fn 无字节upsert保留旧头像() {
        let pool = test_pool().await;
        upsert_profile(&pool, "G", "U", "n", Some(b"old"))
            .await
            .unwrap();
        upsert_profile(&pool, "G", "U", "n2", None).await.unwrap();
        assert_eq!(
            &*load_profile(&pool, "G", "U").await.unwrap().avatar,
            b"old"
        );
    }

    #[tokio::test]
    async fn 清扫删除超窗保留活跃() {
        let pool = test_pool().await;
        upsert_profile(&pool, "G", "live", "n", Some(b"a"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO MEMBER_PROFILE (group_id, user_id, nickname, avatar, saved_at)
             VALUES ('G', 'old', 'n', NULL, ?)",
        )
        .bind(epoch_now() - PROFILE_RETENTION_SECS - 1)
        .execute(&pool)
        .await
        .unwrap();

        delete_expired_profiles(&pool).await.unwrap();

        assert!(load_profile(&pool, "G", "old").await.is_none());
        assert!(load_profile(&pool, "G", "live").await.is_some());
    }
}
