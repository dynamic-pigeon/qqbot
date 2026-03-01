use std::path::Path;

use anyhow::Result;
use kovi::tokio::sync::OnceCell;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

static SQLITE_POOL: OnceCell<SqlitePool> = OnceCell::const_new();

pub(crate) async fn init_db(path: &Path) -> Result<()> {
    SQLITE_POOL
        .get_or_try_init(async || {
            SqlitePoolOptions::new()
                .max_connections(2)
                .connect_lazy(path.to_str().unwrap())
        })
        .await?;

    init_table().await?;
    Ok(())
}

pub(crate) async fn add_msg(group_id: i64, user_id: i64, msg: &str) -> Result<()> {
    let timestamp = chrono::Local::now().timestamp();
    let conn = get_pool().await?;
    sqlx::query("INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES (?, ?, ?, ?)")
        .bind(group_id)
        .bind(user_id)
        .bind(msg)
        .bind(timestamp)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn msg_count_with_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<(i64, u32)>> {
    let conn = get_pool().await?;
    let rows: Vec<(i64, u32)> = sqlx::query_as(
        "
SELECT user_id, COUNT(*) as count FROM MSG 
    WHERE group_id = ? AND timestamp BETWEEN ? AND ? 
    GROUP BY user_id
",
    )
    .bind(group_id)
    .bind(start_time)
    .bind(end_time)
    .fetch_all(conn)
    .await?;

    Ok(rows)
}

pub(crate) async fn select_from_time_range(
    group_id: i64,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<String>> {
    let conn = get_pool().await?;
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
    let conn = get_pool().await?;
    sqlx::query(
        "
CREATE TABLE IF NOT EXISTS MSG (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    msg TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);

-- 适合时间范围查询与分组统计
CREATE INDEX IF NOT EXISTS idx_msg_group_time_user
ON MSG (group_id, timestamp, user_id);

-- 仅用于时间范围拉取消息
CREATE INDEX IF NOT EXISTS idx_msg_group_time
ON MSG (group_id, timestamp);
",
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[inline(always)]
async fn get_pool() -> Result<&'static SqlitePool> {
    let pool = SQLITE_POOL
        .get()
        .ok_or_else(|| anyhow::anyhow!("数据库未初始化"))?;
    Ok(pool)
}
