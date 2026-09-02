use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use kovi::tokio::sync::Mutex;

use crate::similar::{GroupKind, SimilarGroup};

pub const NEXT_PAGE_ARG: &str = "下一组";

/// 标题里的「2/5」对应组号 `2`。纯数字，避免和「90%」抢参数。
pub fn parse_group_index(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok().filter(|index| *index >= 1)
}
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
/// QQ 一条大约 20 张封顶；9 张给查重对照留余量，也避免 base64 消息体过大。
const MAX_IMAGES_PER_MESSAGE: usize = 9;
/// 原始字节。base64 后大约 11 MiB，留在常见 WebSocket 16 MiB 单帧之下。
const MAX_BYTES_PER_MESSAGE: usize = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ScanKey {
    pub group_id: i64,
    pub user_id: i64,
    pub library: String,
}

struct ScanState {
    groups: Vec<SimilarGroup>,
    next: usize,
    last_used: Instant,
}

pub struct ScanSessions {
    /// 命令 handler 是 async 的，这里用 tokio Mutex，避免 std 锁卡住 runtime。
    inner: Mutex<HashMap<ScanKey, ScanState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanAdvance {
    Group {
        group: SimilarGroup,
        index: usize,
        total: usize,
    },
    Exhausted,
    OutOfRange {
        total: usize,
    },
}

impl ScanSessions {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn start(&self, key: ScanKey, groups: Vec<SimilarGroup>) {
        let mut inner = self.inner.lock().await;
        expire(&mut inner);
        inner.insert(
            key,
            ScanState {
                groups,
                next: 0,
                last_used: Instant::now(),
            },
        );
    }

    pub async fn advance(&self, key: &ScanKey) -> Option<ScanAdvance> {
        let mut inner = self.inner.lock().await;
        expire(&mut inner);
        let state = inner.get_mut(key)?;
        state.last_used = Instant::now();
        let total = state.groups.len();
        if state.next >= total {
            return Some(ScanAdvance::Exhausted);
        }
        let index = state.next + 1;
        let group = state.groups[state.next].clone();
        state.next += 1;
        Some(ScanAdvance::Group {
            group,
            index,
            total,
        })
    }

    /// 跳到标题里的第 `index` 组（从 1 起）。随后「下一组」从它的下一组继续。
    pub async fn jump(&self, key: &ScanKey, index: usize) -> Option<ScanAdvance> {
        let mut inner = self.inner.lock().await;
        expire(&mut inner);
        let state = inner.get_mut(key)?;
        state.last_used = Instant::now();
        let total = state.groups.len();
        if index == 0 || index > total {
            return Some(ScanAdvance::OutOfRange { total });
        }
        let group = state.groups[index - 1].clone();
        state.next = index;
        Some(ScanAdvance::Group {
            group,
            index,
            total,
        })
    }
}

fn expire(sessions: &mut HashMap<ScanKey, ScanState>) {
    sessions.retain(|_, state| state.last_used.elapsed() < SESSION_TTL);
}

pub fn group_title(kind: GroupKind, index: usize, total: usize, percent: u8) -> String {
    match kind {
        GroupKind::Duplicate => format!("重复 {index}/{total} · 约 {percent}%"),
        GroupKind::Maybe => {
            format!("也许像 {index}/{total} · 约 {percent}%。不确定，别按重复删")
        }
    }
}

pub struct PackedImage {
    pub hash: String,
    pub bytes: Vec<u8>,
}

/// 一条消息塞不下整组时拆开连发，组内图片仍全部发出。
/// 单张超过字节上限时单独成条，这样才能把 15 MiB 的原图发出去。
pub fn packetize_images(images: Vec<PackedImage>) -> Vec<Vec<PackedImage>> {
    let mut packets = Vec::new();
    let mut current: Vec<PackedImage> = Vec::new();
    let mut bytes = 0usize;
    for image in images {
        let size = image.bytes.len();
        let would_overflow = !current.is_empty()
            && (current.len() >= MAX_IMAGES_PER_MESSAGE
                || bytes.saturating_add(size) > MAX_BYTES_PER_MESSAGE);
        if would_overflow {
            packets.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes = bytes.saturating_add(size);
        current.push(image);
    }
    if !current.is_empty() {
        packets.push(current);
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    fn group(hash: &str) -> SimilarGroup {
        SimilarGroup {
            kind: GroupKind::Duplicate,
            hashes: vec![hash.to_owned()],
            percent: 90,
        }
    }

    fn key() -> ScanKey {
        ScanKey {
            group_id: 1,
            user_id: 2,
            library: "猫".into(),
        }
    }

    #[tokio::test]
    async fn start_then_advance_walks_groups_and_exhausts() {
        let sessions = ScanSessions::new();
        let key = key();
        sessions
            .start(key.clone(), vec![group("a"), group("b")])
            .await;

        let ScanAdvance::Group { index, total, .. } = sessions.advance(&key).await.unwrap() else {
            panic!("first");
        };
        assert_eq!((index, total), (1, 2));
        let ScanAdvance::Group { index, .. } = sessions.advance(&key).await.unwrap() else {
            panic!("second");
        };
        assert_eq!(index, 2);
        assert_eq!(sessions.advance(&key).await, Some(ScanAdvance::Exhausted));
        assert!(
            sessions
                .advance(&ScanKey {
                    group_id: 1,
                    user_id: 9,
                    library: "猫".into(),
                })
                .await
                .is_none()
        );

        sessions.start(key.clone(), vec![group("c")]).await;
        let ScanAdvance::Group { group, total, .. } = sessions.advance(&key).await.unwrap() else {
            panic!("restart");
        };
        assert_eq!(total, 1);
        assert_eq!(group.hashes, ["c"]);
    }

    #[tokio::test]
    async fn jump_selects_index_and_next_continues_after_it() {
        let sessions = ScanSessions::new();
        let key = key();
        sessions
            .start(key.clone(), vec![group("a"), group("b"), group("c")])
            .await;
        let ScanAdvance::Group { index, group, .. } = sessions.jump(&key, 2).await.unwrap() else {
            panic!("jump");
        };
        assert_eq!(index, 2);
        assert_eq!(group.hashes, ["b"]);
        let ScanAdvance::Group { index, group, .. } = sessions.advance(&key).await.unwrap() else {
            panic!("after jump");
        };
        assert_eq!(index, 3);
        assert_eq!(group.hashes, ["c"]);
        assert!(matches!(
            sessions.jump(&key, 9).await,
            Some(ScanAdvance::OutOfRange { total: 3 })
        ));
        assert!(matches!(
            sessions.jump(&key, 0).await,
            Some(ScanAdvance::OutOfRange { total: 3 })
        ));
    }

    #[test]
    fn parse_group_index_is_plain_digits() {
        assert_eq!(parse_group_index("3"), Some(3));
        assert_eq!(parse_group_index("03"), Some(3));
        assert_eq!(parse_group_index("0"), None);
        assert_eq!(parse_group_index("第3组"), None);
        assert_eq!(parse_group_index("90%"), None);
        assert_eq!(parse_group_index("下一组"), None);
    }

    fn packed(count: usize, size: usize) -> Vec<PackedImage> {
        (0..count)
            .map(|i| PackedImage {
                hash: format!("{i:064x}"),
                bytes: vec![1; size],
            })
            .collect()
    }

    #[test]
    fn packetize_splits_on_count_and_bytes() {
        let nine = packetize_images(packed(9, 10));
        assert_eq!(nine.len(), 1);
        assert_eq!(nine[0].len(), 9);

        let ten = packetize_images(packed(10, 10));
        assert_eq!(ten.len(), 2);
        assert_eq!(ten[0].len(), 9);
        assert_eq!(ten[1].len(), 1);

        let huge = packetize_images(packed(2, MAX_BYTES_PER_MESSAGE - 1));
        assert_eq!(huge.len(), 2);
        assert_eq!(huge[0].len(), 1);
    }
}
