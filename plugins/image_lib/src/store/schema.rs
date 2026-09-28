//! 图库建表与列迁移。

use sqlx::{Row, SqlitePool};

use super::StoreError;

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
    migrate_fingerprint_width(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS perceptual (
            hash TEXT NOT NULL PRIMARY KEY CHECK (length(hash) = 64),
            dhash BLOB NOT NULL CHECK (length(dhash) = 32),
            phash BLOB NOT NULL CHECK (length(phash) = 32)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 指纹表换代：64-bit 时代的两列 INTEGER，以及 CHECK 写错成 16 字节的
/// BLOB 表（256-bit 指针是 4×u64 = 32 字节，当时插入全被约束拒绝又被
/// OR IGNORE 静默吞掉，表必为空）。两者都直接弃表重建，指纹由 blobs
/// 懒补齐重算。
async fn migrate_fingerprint_width(pool: &SqlitePool) -> Result<(), StoreError> {
    let sql = sqlx::query_scalar::<_, String>(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'perceptual'",
    )
    .fetch_optional(pool)
    .await?;
    let Some(sql) = sql else {
        return Ok(());
    };
    let legacy_integer = sql.contains("INTEGER");
    let legacy_narrow = sql.contains("length(dhash) = 16");
    if legacy_integer || legacy_narrow {
        sqlx::query("DROP TABLE perceptual").execute(pool).await?;
    }
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
