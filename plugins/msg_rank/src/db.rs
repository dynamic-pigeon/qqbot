use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use kovi::chrono;
use kovi::futures_util::TryStreamExt as _;
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
    restrict_database_permissions(path)?;
    SQLITE_POOL
        .get_or_try_init(async || build_pool(path))
        .await?;

    init_table().await?;
    restrict_database_permissions(path)?;
    init_buffer();
    Ok(())
}

fn restrict_database_permissions(path: &Path) -> Result<()> {
    let mut paths = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths.push(sidecar.into());
    }
    for candidate in paths {
        if candidate.exists() {
            utils::restrict_mode_0600(&candidate)?;
        }
    }
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
                    // 数据库恢复后要在一个周期内清空积压：缓冲满时新消息仍会被丢弃，
                    // 只 flush 一批会把恢复拖长到积压量 / 批大小个周期。
                    while !state.is_empty() {
                        let before = state.len();
                        flush_batch(&mut state).await;
                        if state.len() >= before {
                            break;
                        }
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

/// `timestamp` 用事件时刻而不是入库时刻：OCR 下载识别会推迟入库，
/// 23:59 发出的带图消息不应被计入第二天。
pub(crate) fn add_msg(group_id: i64, user_id: i64, msg: String, timestamp: i64) -> Result<()> {
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

/// 时间范围内的消息总数，用作排行头部的总计与占比分母。
pub(crate) async fn msg_count_total_with_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<u32> {
    let conn = get_pool()?;
    let (total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM MSG WHERE group_id = ? AND timestamp BETWEEN ? AND ?")
            .bind(group_id)
            .bind(start_time)
            .bind(end_time)
            .fetch_one(conn)
            .await?;

    Ok(total.max(0) as u32)
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

/// 周报聚合：按用户返回 (user_id, 消息数, 发言覆盖的本地日期数)，消息数降序。
/// 不加 LIMIT：行数以群成员数为上界，榜单、活跃人数与全勤统计共用这一份结果。
pub(crate) async fn msg_count_with_active_days(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<(i64, u32, u32)>> {
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "
SELECT user_id, COUNT(*) AS cnt,
       COUNT(DISTINCT date(timestamp, 'unixepoch', 'localtime')) AS days
FROM MSG
WHERE group_id = ? AND timestamp BETWEEN ? AND ?
GROUP BY user_id
ORDER BY cnt DESC
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .fetch_all(get_pool()?)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(user_id, count, days)| (user_id, count.max(0) as u32, days.max(0) as u32))
        .collect())
}

/// 按本地日期统计消息数，键为 `YYYY-MM-DD`，升序返回；周报每日柱状图用。
pub(crate) async fn msg_count_by_local_date(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<(String, u32)>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "
SELECT date(timestamp, 'unixepoch', 'localtime') AS d, COUNT(*) AS cnt
FROM MSG
WHERE group_id = ? AND timestamp BETWEEN ? AND ?
GROUP BY d
ORDER BY d
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .fetch_all(get_pool()?)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(date, count)| (date, count.max(0) as u32))
        .collect())
}

/// 给定本地小时集合内发言最多的用户；不足 min_count 时返回 None。
/// 夜聊/早起时段共用；IN 列表元素个数随调用方常量变化，用 QueryBuilder 绑定参数构建。
pub(crate) async fn msg_count_top_at_local_hours(
    group_id: i64,
    start_time: i64,
    end_time: i64,
    hours: &[u32],
    min_count: u32,
) -> Result<Option<(i64, u32)>> {
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT user_id, COUNT(*) AS cnt FROM MSG
WHERE group_id = ",
    );
    builder.push_bind(group_id);
    builder.push(" AND timestamp BETWEEN ");
    builder.push_bind(start_time);
    builder.push(" AND ");
    builder.push_bind(end_time);
    builder.push(" AND CAST(strftime('%H', timestamp, 'unixepoch', 'localtime') AS INTEGER) IN (");
    let mut hours_clause = builder.separated(", ");
    for hour in hours {
        hours_clause.push_bind(i64::from(*hour));
    }
    builder.push(") GROUP BY user_id HAVING cnt >= ");
    builder.push_bind(i64::from(min_count));
    builder.push(" ORDER BY cnt DESC LIMIT 1");

    let row: Option<(i64, i64)> = builder
        .build_query_as::<(i64, i64)>()
        .fetch_optional(get_pool()?)
        .await?;

    Ok(row.map(|(user_id, count)| (user_id, count.max(0) as u32)))
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
    let pool = get_pool()?;
    sqlx::query(
        "
CREATE TABLE IF NOT EXISTS MSG (
    group_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    msg TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);
",
    )
    .execute(pool)
    .await?;

    rebuild_msg_without_id_column(pool).await?;

    sqlx::query(
        "
CREATE INDEX IF NOT EXISTS idx_msg_group_time_user
ON MSG (group_id, timestamp, user_id);
",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "
CREATE INDEX IF NOT EXISTS idx_msg_timestamp
ON MSG (timestamp);
",
    )
    .execute(pool)
    .await?;

    sqlx::query("DROP INDEX IF EXISTS idx_msg_group_time;")
        .execute(pool)
        .await?;

    Ok(())
}

async fn rebuild_msg_without_id_column(pool: &SqlitePool) -> Result<()> {
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('MSG')")
        .fetch_all(pool)
        .await?;
    if !columns.iter().any(|name| name == "id") {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DROP TABLE IF EXISTS MSG_new")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "
CREATE TABLE MSG_new (
    group_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    msg TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);
",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "
INSERT INTO MSG_new (group_id, user_id, msg, timestamp)
SELECT group_id, user_id, msg, timestamp FROM MSG;
",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("DROP TABLE MSG").execute(&mut *tx).await?;
    sqlx::query("ALTER TABLE MSG_new RENAME TO MSG")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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

            // 时间戳取近期值：保留期清理会把过老的行当过期删掉，影响下面的计数。
            let now = chrono::Local::now().timestamp();
            add_msg(1, 100, "hello".into(), now - 10).unwrap();
            add_msg(1, 101, "world".into(), now - 5).unwrap();
            add_msg(2, 100, "other".into(), now - 1).unwrap();

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

            // 周报聚合放在既有断言之后：连接池是进程级 OnceCell，并行测试共享同一份数据，
            // 独立测试函数里的插入会打破上面「全表 3 条」的计数。
            assert_weekly_aggregations().await;
        });

        let _ = std::fs::remove_file(&tmp);
    }

    /// 周报查询的固定数据：用户 1 深夜 5 条 + 次日中午 2 条（跨 2 个本地日期），
    /// 用户 2 清晨 3 条 + 深夜 1 条，用户 3 凌晨 1 条。
    async fn assert_weekly_aggregations() {
        use kovi::chrono::{Days, TimeZone as _};

        // 用本地时间反推时间戳，和 SQL 里 'localtime' 的小时口径一致，测试不依赖运行时区。
        let ts = |days_ago: u32, hour: u32, minute: u32| {
            let date = chrono::Local::now().date_naive() - Days::new(u64::from(days_ago));
            let naive = date.and_hms_opt(hour, minute, 0).expect("时间合法");
            chrono::Local
                .from_local_datetime(&naive)
                .single()
                .unwrap_or_else(|| naive.and_utc().with_timezone(&chrono::Local))
                .timestamp()
        };
        let insert = |user_id: i64, timestamp: i64| {
            sqlx::query("INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES (?, ?, ?, ?)")
                .bind(90210_i64)
                .bind(user_id)
                .bind("m")
                .bind(timestamp)
                .execute(get_pool().unwrap())
        };

        for minute in 0..5 {
            insert(1, ts(2, 23, minute)).await.unwrap();
        }
        insert(1, ts(1, 12, 0)).await.unwrap();
        insert(1, ts(1, 12, 5)).await.unwrap();
        for minute in 0..3 {
            insert(2, ts(2, 7, minute)).await.unwrap();
        }
        insert(2, ts(2, 23, 30)).await.unwrap();
        insert(3, ts(1, 0, 30)).await.unwrap();

        let rows = msg_count_with_active_days(90210, 0, i64::MAX)
            .await
            .unwrap();
        assert_eq!(rows, vec![(1, 7, 2), (2, 4, 1), (3, 1, 1)]);

        let daily: std::collections::HashMap<String, u32> =
            msg_count_by_local_date(90210, 0, i64::MAX)
                .await
                .unwrap()
                .into_iter()
                .collect();
        let today = chrono::Local::now().date_naive();
        let day_ago = |days_ago: u32| (today - Days::new(u64::from(days_ago))).format("%Y-%m-%d");
        assert_eq!(daily.get(&day_ago(2).to_string()), Some(&9));
        assert_eq!(daily.get(&day_ago(1).to_string()), Some(&3));

        let night = msg_count_top_at_local_hours(90210, 0, i64::MAX, &[23, 0, 1, 2, 3, 4, 5], 3)
            .await
            .unwrap();
        assert_eq!(night, Some((1, 5)));
        let early = msg_count_top_at_local_hours(90210, 0, i64::MAX, &[6, 7, 8], 3)
            .await
            .unwrap();
        assert_eq!(early, Some((2, 3)));
        // 门槛过滤：时段内没人达到 6 条时不加冕。
        let none = msg_count_top_at_local_hours(90210, 0, i64::MAX, &[6, 7, 8], 6)
            .await
            .unwrap();
        assert_eq!(none, None);
    }
}
