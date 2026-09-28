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
            dhash BLOB NOT NULL CHECK (length(dhash) = 16),
            phash BLOB NOT NULL CHECK (length(phash) = 16)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 64-bit 时代的旧表把指纹存成两列 INTEGER，256-bit 指纹改存 BLOB。
/// 指纹可从 blobs 懒补齐，直接弃表重建，首次查重时重算即可。
async fn migrate_fingerprint_width(pool: &SqlitePool) -> Result<(), StoreError> {
    let rows = sqlx::query("PRAGMA table_info(perceptual)")
        .fetch_all(pool)
        .await?;
    let legacy = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == "dhash")
            && row
                .try_get::<String, _>("type")
                .is_ok_and(|ty| ty == "INTEGER")
    });
    if legacy {
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
