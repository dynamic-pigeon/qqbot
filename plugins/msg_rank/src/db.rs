use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures::TryStreamExt as _;
use kovi::tokio::sync::{OnceCell, mpsc, oneshot};
use kovi::tokio::time::timeout;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

static SQLITE_POOL: OnceCell<SqlitePool> = OnceCell::const_new();
static MSG_SENDER: OnceCell<mpsc::Sender<MsgRecord>> = OnceCell::const_new();
static SHUTDOWN_TX: OnceCell<Mutex<Option<oneshot::Sender<()>>>> = OnceCell::const_new();
static FLUSH_DONE_RX: OnceCell<Mutex<Option<oneshot::Receiver<()>>>> = OnceCell::const_new();

const FLUSH_INTERVAL_SECS: u64 = 5;
const FLUSH_BATCH_SIZE: usize = 100;
const BUFFER_CAPACITY: usize = 10_000;
/// 消费端缓冲上限（条数）。数据库持续写不进去时达到上限即丢弃新消息，
/// 保证内存有界（约 MAX_BUFFERED_RECORDS × 4KB）且发送端不会被阻塞。
const MAX_BUFFERED_RECORDS: usize = 10_000;
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

fn message_retention_secs() -> i64 {
    crate::config::static_config().retention_days.max(1) as i64 * 24 * 60 * 60
}

/// 缓冲区满 / 数据库不可用时的丢弃计数，恢复后由 [`note_recovered`] 清零并汇总上报。
static DROPPED_MESSAGES: AtomicU64 = AtomicU64::new(0);

/// 每 1000 条丢弃报一次，避免数据库故障期间刷日志。
fn note_dropped() {
    let dropped = DROPPED_MESSAGES.fetch_add(1, Ordering::Relaxed) + 1;
    if dropped % 1000 == 1 {
        tracing::error!("消息缓冲区已满（数据库不可用？），已累计丢弃 {dropped} 条消息");
    }
}

fn note_recovered() {
    let dropped = DROPPED_MESSAGES.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        tracing::warn!("数据库写入恢复，故障期间共丢弃 {dropped} 条消息");
    }
}

struct MsgRecord {
    group_id: i64,
    user_id: i64,
    msg: String,
    timestamp: i64,
}

struct BufferState {
    records: Vec<MsgRecord>,
}

impl BufferState {
    fn new() -> Self {
        Self {
            records: Vec::with_capacity(FLUSH_BATCH_SIZE),
        }
    }

    fn push(&mut self, record: MsgRecord) {
        self.records.push(record);
    }

    fn len(&self) -> usize {
        self.records.len()
    }

    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

pub(crate) async fn init_db(path: &Path) -> Result<()> {
    restrict_database_permissions(path).await?;
    SQLITE_POOL
        .get_or_try_init(async || build_pool(path))
        .await?;

    init_table().await?;
    restrict_database_permissions(path).await?;
    init_buffer();
    Ok(())
}

#[cfg(unix)]
async fn restrict_database_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut paths = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths.push(sidecar.into());
    }
    for candidate in paths {
        if candidate.exists() {
            kovi::tokio::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o600))
                .await?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
async fn restrict_database_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn build_pool(path: &Path) -> Result<SqlitePool> {
    let url = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("数据库路径包含非法字符"))?;

    Ok(SqlitePoolOptions::new()
        .max_connections(2)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("PRAGMA journal_mode = WAL;")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA synchronous = NORMAL;")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA busy_timeout = 5000;")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA temp_store = MEMORY;")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy(url)?)
}

fn init_buffer() {
    let (tx, mut rx) = mpsc::channel(BUFFER_CAPACITY);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();

    if MSG_SENDER.set(tx).is_err() {
        return;
    }
    let _ = SHUTDOWN_TX.set(Mutex::new(Some(shutdown_tx)));
    let _ = FLUSH_DONE_RX.set(Mutex::new(Some(done_rx)));

    kovi::tokio::spawn(async move {
        let mut state = BufferState::new();
        let mut interval = kovi::tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
        let mut cleanup_interval = kovi::tokio::time::interval(Duration::from_secs(24 * 60 * 60));

        loop {
            kovi::tokio::select! {
                Some(record) = rx.recv() => {
                    if state.len() >= MAX_BUFFERED_RECORDS {
                        note_dropped();
                    } else {
                        state.push(record);
                        if state.len() >= FLUSH_BATCH_SIZE {
                            flush_batch(&mut state).await;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !state.is_empty() {
                        flush_batch(&mut state).await;
                    }
                }
                _ = cleanup_interval.tick() => {
                    if let Err(e) = delete_expired_messages().await {
                        tracing::error!("清理过期消息失败: {e}");
                    }
                }
                _ = &mut shutdown_rx => {
                    while let Ok(record) = rx.try_recv() {
                        state.push(record);
                    }
                    // flush_batch 失败时会保留批次，靠「长度不再下降」识别 DB 持续故障，
                    // 避免关闭流程无限重试。
                    const MAX_SHUTDOWN_FLUSH_FAILURES: usize = 3;
                    let mut failures = 0;
                    while !state.is_empty() && failures < MAX_SHUTDOWN_FLUSH_FAILURES {
                        let before = state.len();
                        flush_batch(&mut state).await;
                        if state.len() < before {
                            failures = 0;
                        } else {
                            failures += 1;
                        }
                    }
                    if !state.is_empty() {
                        tracing::warn!("关闭刷新未完成，丢弃 {} 条消息", state.len());
                    }
                    let _ = done_tx.send(());
                    break;
                }
            }
        }
    });
}

async fn flush_batch(state: &mut BufferState) {
    if state.is_empty() {
        return;
    }

    let chunk_size = state.len().min(FLUSH_BATCH_SIZE);
    let pool = match get_pool() {
        Ok(pool) => pool,
        Err(e) => {
            // 保留批次，下次周期或新消息到达时自动重试。
            tracing::error!(
                "批量写入失败，无法获取数据库连接，保留 {} 条消息: {}",
                chunk_size,
                e
            );
            return;
        }
    };

    // QueryBuilder 单缓冲构建 SQL，占位符与绑定参数由库生成。
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO MSG (group_id, user_id, msg, timestamp) ",
    );
    builder.push_values(&state.records[..chunk_size], |mut row, record| {
        row.push_bind(record.group_id)
            .push_bind(record.user_id)
            .push_bind(&record.msg)
            .push_bind(record.timestamp);
    });

    match builder.build().execute(pool).await {
        Ok(_) => {
            state.records.drain(..chunk_size);
            note_recovered();
        }
        Err(e) => {
            // 保留批次，避免消息丢失；下次 flush 自动重试。
            tracing::error!("批量写入消息失败，保留 {} 条消息: {}", chunk_size, e);
        }
    }
}

pub(crate) async fn flush_on_shutdown() {
    let maybe_tx = SHUTDOWN_TX.get().and_then(|m| m.lock().ok()?.take());
    let maybe_done = FLUSH_DONE_RX.get().and_then(|m| m.lock().ok()?.take());

    let Some(tx) = maybe_tx else {
        tracing::warn!("消息缓冲区未初始化，无法执行关闭刷新");
        return;
    };
    let Some(done) = maybe_done else {
        tracing::warn!("消息缓冲区关闭完成通道未初始化");
        return;
    };

    if tx.send(()).is_err() {
        tracing::warn!("消息缓冲区任务已停止，无法触发关闭刷新");
        return;
    }

    match timeout(SHUTDOWN_FLUSH_TIMEOUT, done).await {
        Ok(Ok(())) => tracing::info!("消息缓冲区关闭前刷新完成"),
        Ok(Err(_)) => tracing::warn!("消息缓冲区关闭通道已关闭"),
        Err(_) => tracing::warn!("消息缓冲区关闭前刷新超时"),
    }
}

pub(crate) fn add_msg(group_id: i64, user_id: i64, msg: String) -> Result<()> {
    let timestamp = chrono::Local::now().timestamp();
    let record = MsgRecord {
        group_id,
        user_id,
        msg,
        timestamp,
    };
    let sender = MSG_SENDER
        .get()
        .ok_or_else(|| anyhow::anyhow!("消息缓冲区未初始化"))?;
    match sender.try_send(record) {
        Ok(()) => Ok(()),
        // 缓冲区满说明数据库持续写不进去：丢弃并计数，不能阻塞群消息处理路径。
        Err(mpsc::error::TrySendError::Full(_)) => {
            note_dropped();
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(anyhow::anyhow!("消息缓冲区已关闭")),
    }
}

pub(crate) async fn msg_count_top_with_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
    limit: i64,
) -> Result<Vec<(i64, u32)>> {
    let conn = get_pool()?;
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "
SELECT user_id, COUNT(*) as count FROM MSG
    WHERE group_id = ? AND timestamp BETWEEN ? AND ?
    GROUP BY user_id
    ORDER BY count DESC
    LIMIT ?
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .bind(limit)
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(user_id, count)| (user_id, count as u32))
        .collect())
}

pub(crate) async fn select_text_from_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
    max_bytes: usize,
) -> Result<String> {
    let conn = get_pool()?;
    let mut rows = sqlx::query_as::<_, (String,)>(
        "
SELECT msg FROM MSG
    WHERE group_id = ? AND timestamp BETWEEN ? AND ?
    ORDER BY timestamp DESC
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .fetch(conn);

    let mut text = String::with_capacity(max_bytes.min(64 * 1024));
    while let Some((message,)) = rows.try_next().await? {
        let separator_len = usize::from(!text.is_empty());
        let remaining = max_bytes.saturating_sub(text.len() + separator_len);
        if remaining == 0 {
            break;
        }
        if separator_len != 0 {
            text.push(' ');
        }
        if message.len() <= remaining {
            text.push_str(&message);
            continue;
        }
        let mut boundary = remaining;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.push_str(&message[..boundary]);
        break;
    }
    Ok(text)
}

async fn delete_expired_messages() -> Result<u64> {
    let cutoff = chrono::Local::now().timestamp() - message_retention_secs();
    let result = sqlx::query("DELETE FROM MSG WHERE timestamp < ?")
        .bind(cutoff)
        .execute(get_pool()?)
        .await?;
    if result.rows_affected() > 0 {
        tracing::info!("已清理 {} 条过期消息", result.rows_affected());
    }
    Ok(result.rows_affected())
}

async fn init_table() -> Result<()> {
    let conn = get_pool()?;

    sqlx::query(
        "
CREATE TABLE IF NOT EXISTS MSG (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    msg TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);
",
    )
    .execute(conn)
    .await?;

    sqlx::query(
        "
CREATE INDEX IF NOT EXISTS idx_msg_group_time_user
ON MSG (group_id, timestamp, user_id);
",
    )
    .execute(conn)
    .await?;

    sqlx::query("DROP INDEX IF EXISTS idx_msg_group_time;")
        .execute(conn)
        .await?;

    Ok(())
}

#[inline(always)]
fn get_pool() -> Result<&'static SqlitePool> {
    SQLITE_POOL
        .get()
        .ok_or_else(|| anyhow::anyhow!("数据库未初始化"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_insert_and_shutdown_flush() {
        let tmp = std::env::temp_dir().join(format!("msg_rank_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::File::create(&tmp).unwrap();

        let rt = kovi::tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            init_db(&tmp).await.unwrap();

            add_msg(1, 100, "hello".into()).unwrap();
            add_msg(1, 101, "world".into()).unwrap();
            add_msg(2, 100, "other".into()).unwrap();

            flush_on_shutdown().await;

            let group1 = msg_count_top_with_time_range(1, 0, i64::MAX, 10)
                .await
                .unwrap();
            assert_eq!(group1.len(), 2);

            let group2 = msg_count_top_with_time_range(2, 0, i64::MAX, 10)
                .await
                .unwrap();
            assert_eq!(group2.len(), 1);

            let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM MSG")
                .fetch_one(get_pool().unwrap())
                .await
                .unwrap();
            assert_eq!(total.0, 3);

            let text = select_text_from_time_range(1, 0, i64::MAX, 7)
                .await
                .unwrap();
            assert!(text.len() <= 7);
            assert!(text.is_char_boundary(text.len()));

            sqlx::query("INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES (?, ?, ?, ?)")
                .bind(1_i64)
                .bind(100_i64)
                .bind("expired")
                .bind(chrono::Local::now().timestamp() - message_retention_secs() - 1)
                .execute(get_pool().unwrap())
                .await
                .unwrap();
            assert_eq!(delete_expired_messages().await.unwrap(), 1);
        });

        let _ = std::fs::remove_file(&tmp);
    }
}
