use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

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
const MAX_IMAGES_PER_MESSAGE: usize = 3;
const MAX_BYTES_PER_MESSAGE: usize = 2 * 1024 * 1024;

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

    pub fn start(&self, key: ScanKey, groups: Vec<SimilarGroup>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    pub fn advance(&self, key: &ScanKey) -> Option<ScanAdvance> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    pub fn jump(&self, key: &ScanKey, index: usize) -> Option<ScanAdvance> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// 一条消息塞不下整组时拆开连发，组内图片仍全部发出。
pub fn packetize_images(images: Vec<Vec<u8>>) -> Vec<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    let mut current: Vec<Vec<u8>> = Vec::new();
    let mut bytes = 0usize;
    for image in images {
        let size = image.len();
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

    #[test]
    fn start_then_advance_walks_groups_and_exhausts() {
        let sessions = ScanSessions::new();
        let key = key();
        sessions.start(key.clone(), vec![group("a"), group("b")]);

        let ScanAdvance::Group { index, total, .. } = sessions.advance(&key).unwrap() else {
            panic!("first");
        };
        assert_eq!((index, total), (1, 2));
        let ScanAdvance::Group { index, .. } = sessions.advance(&key).unwrap() else {
            panic!("second");
        };
        assert_eq!(index, 2);
        assert_eq!(sessions.advance(&key), Some(ScanAdvance::Exhausted));
        assert!(
            sessions
                .advance(&ScanKey {
                    group_id: 1,
                    user_id: 9,
                    library: "猫".into(),
                })
                .is_none()
        );

        sessions.start(key.clone(), vec![group("c")]);
        let ScanAdvance::Group { group, total, .. } = sessions.advance(&key).unwrap() else {
            panic!("restart");
        };
        assert_eq!(total, 1);
        assert_eq!(group.hashes, ["c"]);
    }

    #[test]
    fn jump_selects_index_and_next_continues_after_it() {
        let sessions = ScanSessions::new();
        let key = key();
        sessions.start(key.clone(), vec![group("a"), group("b"), group("c")]);
        let ScanAdvance::Group { index, group, .. } = sessions.jump(&key, 2).unwrap() else {
            panic!("jump");
        };
        assert_eq!(index, 2);
        assert_eq!(group.hashes, ["b"]);
        let ScanAdvance::Group { index, group, .. } = sessions.advance(&key).unwrap() else {
            panic!("after jump");
        };
        assert_eq!(index, 3);
        assert_eq!(group.hashes, ["c"]);
        assert!(matches!(
            sessions.jump(&key, 9),
            Some(ScanAdvance::OutOfRange { total: 3 })
        ));
        assert!(matches!(
            sessions.jump(&key, 0),
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

    #[test]
    fn packetize_splits_on_count_and_bytes() {
        let small = vec![vec![1; 10]; 7];
        assert_eq!(packetize_images(small).len(), 3);

        let huge = vec![vec![0; MAX_BYTES_PER_MESSAGE - 1]; 2];
        let packets = packetize_images(huge);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].len(), 1);
    }
}
