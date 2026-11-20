use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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

// ── 盘上缓存 ───────────────────────────────────────────────────────────────
//
// 内存缓存重启即空；官方栈成员信息只能来自事件流、头像 URL 又有时效，
// 重启后没再发言的成员昵称会退化成 openid 串。昵称与头像落盘兜底：
// `avatars/<group>/<user>.json` 存昵称与落盘时间，`avatars/<group>/<user>`
// 存头像字节。

/// 盘缓存根目录，插件初始化时设置；未设置（单测环境）时禁用。
static DISK_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 落盘时间超过该窗的条目视为不活跃，清扫删除。榜上成员必然活跃：
/// 每次成功拉取都会刷新 .json 的落盘时间，不会被误清。
const DISK_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(serde::Serialize, serde::Deserialize)]
struct ProfileRecord {
    nickname: String,
    saved_at: u64,
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 设置盘缓存目录并启动每日清扫（interval 首个 tick 立即触发，启动即扫一遍）。
pub(crate) fn init(data_dir: &Path) {
    if DISK_ROOT.set(data_dir.join("avatars")).is_err() {
        return;
    }
    let Some(root) = DISK_ROOT.get() else {
        return;
    };
    let _ = std::fs::create_dir_all(root);
    let sweep_root = root.clone();
    kovi::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            ticker.tick().await;
            sweep_expired(&sweep_root);
        }
    });
}

/// 拉取成功后落盘：.json 每次都重写（落盘时间是清扫的活跃凭据），
/// 头像字节只在缺失或长度变化时重写，避免逐小时无效写放大。
async fn persist_profile(root: &Path, group: &str, user: &str, nickname: &str, avatar: &[u8]) {
    let dir = root.join(group);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!("创建头像缓存目录失败: {e}");
        return;
    }
    let record = ProfileRecord {
        nickname: nickname.to_owned(),
        saved_at: epoch_now(),
    };
    if let Ok(bytes) = kovi::serde_json::to_vec(&record)
        && let Err(e) = write_atomic(&dir.join(format!("{user}.json")), &bytes).await
    {
        tracing::warn!("写入昵称缓存失败 user={user}: {e}");
    }
    if avatar.is_empty() {
        return;
    }
    let bin = dir.join(user);
    let unchanged = tokio::fs::metadata(&bin)
        .await
        .map(|meta| meta.len() as usize == avatar.len())
        .unwrap_or(false);
    if !unchanged
        && let Err(e) = write_atomic(&bin, avatar).await
    {
        tracing::warn!("写入头像缓存失败 user={user}: {e}");
    }
}

/// 只取盘上的头像字节，头像下载失败时兜底。
async fn disk_avatar(group: &str, user: &str) -> bytes::Bytes {
    let Some(root) = DISK_ROOT.get() else {
        return bytes::Bytes::new();
    };
    tokio::fs::read(root.join(group).join(user))
        .await
        .map(bytes::Bytes::from)
        .unwrap_or_default()
}

/// 成员信息查不到时从盘上恢复（昵称 + 头像，容忍过期内容）。
async fn load_cached(group: &str, user: &str) -> Option<UserInfo> {
    let root = DISK_ROOT.get()?;
    load_profile(root, group, user).await
}

async fn load_profile(root: &Path, group: &str, user: &str) -> Option<UserInfo> {
    let dir = root.join(group);
    let bytes = tokio::fs::read(dir.join(format!("{user}.json"))).await.ok()?;
    let record: ProfileRecord = kovi::serde_json::from_slice(&bytes).ok()?;
    let avatar = tokio::fs::read(dir.join(user)).await.unwrap_or_default();
    Some(UserInfo {
        user_key: user.to_owned(),
        nickname: record.nickname,
        avatar: avatar.into(),
        fetched_at: Instant::now(),
    })
}

/// 先写临时文件再原子改名，避免清扫或崩溃读到半截文件。
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_owned());
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// 清扫：删除落盘时间超窗的 .json 及其头像、没有存活 .json 的孤儿头像、
/// 一天前的 .tmp 残片；收尾移除空群目录。
fn sweep_expired(root: &Path) {
    let cutoff = epoch_now().saturating_sub(DISK_RETENTION_SECS);
    let Ok(groups) = std::fs::read_dir(root) else {
        return;
    };
    let mut removed = 0usize;
    for group in groups.flatten() {
        let group_path = group.path();
        let Ok(entries) = std::fs::read_dir(&group_path) else {
            continue;
        };
        let mut kept: Vec<std::ffi::OsString> = Vec::new();
        let mut strays: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if let Some(user) = name.strip_suffix(".json") {
                let fresh = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| kovi::serde_json::from_slice::<ProfileRecord>(&bytes).ok())
                    .is_some_and(|record| record.saved_at >= cutoff);
                if fresh {
                    kept.push(user.to_owned().into());
                } else {
                    removed += std::fs::remove_file(&path).is_ok() as usize;
                }
            } else if name.ends_with(".tmp") {
                let stale = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| {
                        modified
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .ok()
                    })
                    .is_some_and(|since| since.as_secs() + 24 * 60 * 60 < epoch_now());
                if stale {
                    removed += std::fs::remove_file(&path).is_ok() as usize;
                }
            } else {
                strays.push(path);
            }
        }
        for avatar in strays {
            let is_kept = avatar
                .file_name()
                .is_some_and(|name| kept.iter().any(|k| k.as_os_str() == name));
            if !is_kept {
                removed += std::fs::remove_file(&avatar).is_ok() as usize;
            }
        }
        if std::fs::read_dir(&group_path)
            .map(|mut rest| rest.next().is_none())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&group_path);
        }
    }
    if removed > 0 {
        tracing::info!("头像缓存清扫: 删除 {removed} 个条目");
    }
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
            // 成员信息不可用（官方栈重启后该成员未再发言）：盘上缓存兜底。
            if let Some(cached) = load_cached(group_key, user_key).await {
                tracing::warn!("成员信息不可用，使用盘上缓存 user={user_key}: {e}");
                return Ok(cached);
            }
            return Err(e);
        }
    };

    let avatar = match profile.avatar.as_deref() {
        Some(url) => match fetch_avatar_with_retry(url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("获取头像失败，user={}: {}", user_key, e);
                disk_avatar(group_key, user_key).await
            }
        },
        None => bytes::Bytes::new(),
    };

    let nickname = match profile.card.as_deref() {
        Some(card) if !card.is_empty() => card.to_owned(),
        _ => profile.nickname,
    };
    if let Some(root) = DISK_ROOT.get() {
        persist_profile(root, group_key, user_key, &nickname, &avatar).await;
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

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "msg-rank-avatar-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn 落盘昵称与头像后可整条恢复() {
        let root = temp_root("roundtrip");
        persist_profile(&root, "G", "U", "昵称", b"avatar-bytes").await;
        let info = load_profile(&root, "G", "U").await.unwrap();
        assert_eq!(info.nickname, "昵称");
        assert_eq!(&*info.avatar, b"avatar-bytes");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn 头像等长不重写变长重写() {
        let root = temp_root("dedup");
        persist_profile(&root, "G", "U", "n", b"aaaa").await;
        persist_profile(&root, "G", "U", "n", b"bbbb").await;
        assert_eq!(std::fs::read(root.join("G/U")).unwrap(), b"aaaa");
        persist_profile(&root, "G", "U", "n", b"cc").await;
        assert_eq!(std::fs::read(root.join("G/U")).unwrap(), b"cc");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn 清扫删除超窗与孤儿保留活跃() {
        let root = temp_root("sweep");
        // 活跃条目：新鲜落盘时间 + 头像。
        persist_profile(&root, "G", "live", "n", b"avatar").await;
        // 超窗条目：落盘时间在窗口之外。
        let stale = ProfileRecord {
            nickname: "n".to_owned(),
            saved_at: epoch_now() - DISK_RETENTION_SECS - 1,
        };
        std::fs::write(root.join("G/old.json"), kovi::serde_json::to_vec(&stale).unwrap()).unwrap();
        std::fs::write(root.join("G/old"), b"stale-avatar").unwrap();
        // 孤儿头像：没有对应 .json。
        std::fs::write(root.join("G/orphan"), b"orphan").unwrap();

        sweep_expired(&root);

        assert!(root.join("G/live.json").is_file());
        assert_eq!(std::fs::read(root.join("G/live")).unwrap(), b"avatar");
        assert!(!root.join("G/old.json").exists());
        assert!(!root.join("G/old").exists());
        assert!(!root.join("G/orphan").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn 无昵称记录时不作整条恢复() {
        let root = temp_root("miss");
        assert!(load_profile(&root, "G", "nobody").await.is_none());
        // 只有头像没有 .json：不算可恢复的条目。
        std::fs::create_dir_all(root.join("G")).unwrap();
        std::fs::write(root.join("G/U"), b"x").unwrap();
        assert!(load_profile(&root, "G", "U").await.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
