use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use kovi::tokio::sync::OnceCell;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

static SQLITE_POOL: OnceCell<SqlitePool> = OnceCell::const_new();

pub(crate) async fn init_db(path: &Path) -> Result<()> {
    SQLITE_POOL
        .get_or_try_init(async || build_pool(path))
        .await?;

    init_table().await?;
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

pub(crate) async fn add_msg(group_id: i64, user_id: i64, msg: &str) -> Result<()> {
    let timestamp = chrono::Local::now().timestamp();
    let conn = get_pool()?;
    sqlx::query("INSERT INTO MSG (group_id, user_id, msg, timestamp) VALUES (?, ?, ?, ?)")
        .bind(group_id)
        .bind(user_id)
        .bind(msg)
        .bind(timestamp)
        .execute(conn)
        .await?;
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

    // 清理旧版冗余索引：新索引已覆盖该索引的查询场景。
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
