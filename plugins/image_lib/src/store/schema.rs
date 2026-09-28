//! 图库建表与列迁移。

use sqlx::{Row, SqlitePool};

use super::StoreError;

/// 指纹表结构版本，存进 schema_meta，换代只看它：
/// v1 是 64-bit 指纹的两列 INTEGER；v2 是 256-bit 指纹的两列 32 字节 BLOB。
const FINGERPRINT_SCHEMA: &str = "v2";

pub(super) async fn init_schema(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS images (
            library TEXT NOT NULL,
            hash TEXT NOT NULL CHECK (length(hash) = 64),
            size INTEGER NOT NULL CHECK (size > 0),
            draw_count INTEGER NOT NULL DEFAULT 0 CHECK (draw_count >= 0),
            PRIMARY KEY (library, hash)
        )",
    )
    .execute(pool)
    .await?;
    ensure_draw_count_column(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS aliases (
            alias TEXT NOT NULL PRIMARY KEY,
            target TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_images_hash ON images(hash)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_aliases_target ON aliases(target)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_meta (
            key TEXT NOT NULL PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    migrate_fingerprint_schema(pool).await?;
    Ok(())
}

/// 没有版本记录的库——全新库、v1 的 INTEGER 表、CHECK 误写成 16 的
/// 中间态——统一弃表重建；指纹可从 blobs 懒补齐，无损。库里的版本
/// 比代码新时报错拒开，防止旧二进制把新表写坏。
async fn migrate_fingerprint_schema(pool: &SqlitePool) -> Result<(), StoreError> {
    let current =
        sqlx::query_scalar::<_, String>("SELECT value FROM schema_meta WHERE key = 'perceptual'")
            .fetch_optional(pool)
            .await?;
    match current.as_deref() {
        Some(FINGERPRINT_SCHEMA) => return Ok(()),
        Some(newer) => {
            return Err(StoreError::Other(anyhow::anyhow!(
                "指纹表版本 {newer} 比当前代码支持的 {FINGERPRINT_SCHEMA} 新，请先升级程序"
            )));
        }
        None => {}
    }
    sqlx::query("DROP TABLE IF EXISTS perceptual")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE perceptual (
            hash TEXT NOT NULL PRIMARY KEY CHECK (length(hash) = 64),
            dhash BLOB NOT NULL CHECK (length(dhash) = 32),
            phash BLOB NOT NULL CHECK (length(phash) = 32)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO schema_meta (key, value) VALUES ('perceptual', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(FINGERPRINT_SCHEMA)
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_draw_count_column(pool: &SqlitePool) -> Result<(), StoreError> {
    let rows = sqlx::query("PRAGMA table_info(images)")
        .fetch_all(pool)
        .await?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == "draw_count")
    });
    if !exists {
        sqlx::query("ALTER TABLE images ADD COLUMN draw_count INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await?;
    }
    Ok(())
}
