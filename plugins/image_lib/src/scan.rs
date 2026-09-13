use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::similar::{GroupKind, SimilarGroup};

pub const NEXT_PAGE_ARG: &str = "下一组";
/// 「查重」一次放进一条聊天记录的组数；「下一组」翻下一批。
pub const GROUPS_PER_PAGE: usize = 5;

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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ScanKey, ScanState>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn start(&self, key: ScanKey, groups: Vec<SimilarGroup>) {
        let mut inner = self.lock();
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
        let mut inner = self.lock();
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
        let mut inner = self.lock();
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
        GroupKind::Duplicate => group_node_name(kind, index, total, percent),
        GroupKind::Maybe => {
            format!("也许像 {index}/{total} · 约 {percent}%。不确定，别按重复删")
        }
    }
}

/// 合并转发节点昵称。Maybe 组的警告放在正文里，避免昵称被截断。
pub fn group_node_name(kind: GroupKind, index: usize, total: usize, percent: u8) -> String {
    match kind {
        GroupKind::Duplicate => format!("重复 {index}/{total} · 约 {percent}%"),
        GroupKind::Maybe => format!("也许像 {index}/{total} · 约 {percent}%"),
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

pub struct ForwardPacket {
    pub name: String,
    pub caption: String,
    pub images: Vec<PackedImage>,
}

impl ForwardPacket {
    pub fn byte_len(&self) -> usize {
        self.images.iter().map(|image| image.bytes.len()).sum()
    }
}

/// 一组图可能拆成多条节点；第一条带标题，后续标「（续）」。
pub fn packets_for_group(
    title: String,
    name: String,
    images: Vec<PackedImage>,
) -> Vec<ForwardPacket> {
    let mut packets = packetize_images(images);
    if packets.is_empty() {
        return Vec::new();
    }
    let first = packets.remove(0);
    let mut out = Vec::with_capacity(packets.len() + 1);
    out.push(ForwardPacket {
        name: name.clone(),
        caption: title,
        images: first,
    });
    out.extend(packets.into_iter().map(|images| ForwardPacket {
        name: name.clone(),
        caption: "（续）".to_owned(),
        images,
    }));
    out
}

/// 一条聊天记录塞不下时拆开连发。整包超过字节上限时单独成条。
pub fn chunk_forward_packets(packets: Vec<ForwardPacket>) -> Vec<Vec<ForwardPacket>> {
    let mut chunks = Vec::new();
    let mut current: Vec<ForwardPacket> = Vec::new();
    let mut bytes = 0usize;
    for packet in packets {
        let size = packet.byte_len();
        let would_overflow =
            !current.is_empty() && bytes.saturating_add(size) > MAX_BYTES_PER_MESSAGE;
        if would_overflow {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes = bytes.saturating_add(size);
        current.push(packet);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
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

    #[test]
    fn packets_for_group_marks_continuations() {
        let packets = packets_for_group(
            "重复 1/2 · 约 90%".into(),
            "重复 1/2 · 约 90%".into(),
            packed(10, 10),
        );
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].caption, "重复 1/2 · 约 90%");
        assert_eq!(packets[1].caption, "（续）");
        assert_eq!(packets[0].images.len(), 9);
        assert_eq!(packets[1].images.len(), 1);
    }

    #[test]
    fn chunk_forward_splits_on_bytes() {
        let group = packets_for_group(
            "重复 1/2 · 约 90%".into(),
            "重复 1/2 · 约 90%".into(),
            packed(2, MAX_BYTES_PER_MESSAGE - 1),
        );
        let chunks = chunk_forward_packets(group);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn maybe_node_name_drops_warning() {
        assert_eq!(
            group_node_name(GroupKind::Maybe, 2, 5, 80),
            "也许像 2/5 · 约 80%"
        );
        assert!(group_title(GroupKind::Maybe, 2, 5, 80).contains("不确定"));
    }
}
