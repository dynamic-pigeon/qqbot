use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
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
const MAX_FLUSH_RETRIES: u8 = 3;
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

struct MsgRecord {
    group_id: i64,
    user_id: i64,
    msg: String,
    timestamp: i64,
}

struct BufferState {
    records: Vec<MsgRecord>,
    retry_count: u8,
}

impl BufferState {
    fn new() -> Self {
        Self {
            records: Vec::with_capacity(FLUSH_BATCH_SIZE),
            retry_count: 0,
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
    SQLITE_POOL
        .get_or_try_init(async || build_pool(path))
        .await?;

    init_table().await?;
    init_buffer();
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

        loop {
            kovi::tokio::select! {
                Some(record) = rx.recv() => {
                    state.push(record);
                    if state.len() >= FLUSH_BATCH_SIZE {
                        flush_batch(&mut state).await;
                    }
                }
                _ = interval.tick() => {
                    if !state.is_empty() {
                        flush_batch(&mut state).await;
                    }
                }
                _ = &mut shutdown_rx => {
                    while let Ok(record) = rx.try_recv() {
                        state.push(record);
                        if state.len() >= FLUSH_BATCH_SIZE {
                            flush_batch(&mut state).await;
                        }
                    }
                    while !state.is_empty() {
                        flush_batch(&mut state).await;
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
            tracing::error!("批量写入失败，无法获取数据库连接: {}", e);
            state.retry_count = state.retry_count.saturating_add(1);
            if state.retry_count >= MAX_FLUSH_RETRIES {
                tracing::error!(
                    "批量写入在 {} 次重试后仍失败，丢弃 {} 条消息",
                    MAX_FLUSH_RETRIES,
                    chunk_size
                );
                state.records.drain(..chunk_size);
                state.retry_count = 0;
            }
            return;
        }
    };

    let placeholders: Vec<String> = (0..chunk_size)
        .map(|_| "(?, ?, ?, ?)".to_string())
        .collect();
    let sql = format!(
        "INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES {}",
        placeholders.join(", ")
    );

    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for record in &state.records[..chunk_size] {
        query = query
            .bind(record.group_id)
            .bind(record.user_id)
            .bind(&record.msg)
            .bind(record.timestamp);
    }

    match query.execute(pool).await {
        Ok(_) => {
            state.records.drain(..chunk_size);
            state.retry_count = 0;
        }
        Err(e) => {
            tracing::error!("批量写入消息失败: {}", e);
            state.retry_count = state.retry_count.saturating_add(1);
            if state.retry_count >= MAX_FLUSH_RETRIES {
                tracing::error!(
                    "批量写入消息在 {} 次重试后仍然失败，丢弃 {} 条消息",
                    MAX_FLUSH_RETRIES,
                    chunk_size
                );
                state.records.drain(..chunk_size);
                state.retry_count = 0;
            }
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

pub(crate) async fn add_msg(group_id: i64, user_id: i64, msg: String) -> Result<()> {
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
    sender
        .send(record)
        .await
        .map_err(|_| anyhow::anyhow!("消息缓冲区已关闭"))?;
    Ok(())
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

pub(crate) async fn select_from_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<String>> {
    let conn = get_pool()?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "
SELECT msg FROM MSG
    WHERE group_id = ? AND timestamp BETWEEN ? AND ?
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .fetch_all(conn)
    .await?;

    Ok(rows.into_iter().map(|row| row.0).collect())
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
    fn test_build_placeholders_for_batch_insert() {
        let placeholders: Vec<String> = (0..3).map(|_| "(?, ?, ?, ?)".to_string()).collect();
        let sql = format!(
            "INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES {}",
            placeholders.join(", ")
        );
        assert_eq!(
            sql,
            "INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES (?, ?, ?, ?), (?, ?, ?, ?), (?, ?, ?, ?)"
        );
    }

    #[test]
    fn test_batch_insert_and_shutdown_flush() {
        let tmp = std::env::temp_dir().join(format!("msg_rank_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::File::create(&tmp).unwrap();

        let rt = kovi::tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            init_db(&tmp).await.unwrap();

            add_msg(1, 100, "hello".into()).await.unwrap();
            add_msg(1, 101, "world".into()).await.unwrap();
            add_msg(2, 100, "other".into()).await.unwrap();

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
        });

        let _ = std::fs::remove_file(&tmp);
    }
}
