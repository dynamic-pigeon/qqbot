//! 图库 SQL 层：所有直接面对 sqlite 的查询与变更。

use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

use super::{StagedImage, StoreError};
use crate::similar::Fingerprint;

pub(super) async fn resolve_library(pool: &SqlitePool, name: &str) -> Result<String, StoreError> {
    let target = sqlx::query_scalar::<_, String>("SELECT target FROM aliases WHERE alias = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(target.unwrap_or_else(|| name.to_owned()))
}

pub(super) async fn library_exists(pool: &SqlitePool, name: &str) -> Result<bool, StoreError> {
    let found = sqlx::query_scalar::<_, i64>("SELECT 1 FROM images WHERE library = ? LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

pub(super) async fn library_hashes(
    pool: &SqlitePool,
    library: &str,
) -> Result<HashSet<String>, StoreError> {
    let hashes = sqlx::query_scalar::<_, String>("SELECT hash FROM images WHERE library = ?")
        .bind(library)
        .fetch_all(pool)
        .await?;
    Ok(hashes.into_iter().collect())
}

pub(super) async fn unique_image_bytes(pool: &SqlitePool) -> Result<u64, StoreError> {
    let bytes = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(size) FROM (SELECT hash, MAX(size) AS size FROM images GROUP BY hash)",
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    Ok(bytes as u64)
}

pub(super) async fn additional_unique_bytes(
    pool: &SqlitePool,
    to_insert: &[&StagedImage],
) -> Result<u64, StoreError> {
    if to_insert.is_empty() {
        return Ok(0);
    }
    let mut builder =
        sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT DISTINCT hash FROM images WHERE hash IN (");
    {
        let mut separated = builder.separated(", ");
        for image in to_insert {
            separated.push_bind(&image.hash);
        }
    }
    builder.push(")");
    let present: HashSet<String> = builder
        .build_query_scalar()
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    Ok(to_insert
        .iter()
        .filter(|image| !present.contains(&image.hash))
        .map(|image| image.size)
        .sum())
}

pub(super) async fn hashes_only_in_library(
    pool: &SqlitePool,
    library: &str,
) -> Result<Vec<String>, StoreError> {
    let hashes = sqlx::query_scalar::<_, String>(
        "SELECT mine.hash FROM images AS mine
         WHERE mine.library = ?
           AND NOT EXISTS (
               SELECT 1 FROM images AS other
               WHERE other.hash = mine.hash AND other.library != mine.library
           )",
    )
    .bind(library)
    .fetch_all(pool)
    .await?;
    Ok(hashes)
}

pub(super) async fn hash_still_used(pool: &SqlitePool, hash: &str) -> Result<bool, StoreError> {
    let found = sqlx::query_scalar::<_, i64>("SELECT 1 FROM images WHERE hash = ? LIMIT 1")
        .bind(hash)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

pub(super) async fn insert_fingerprints(
    pool: &SqlitePool,
    fingerprints: &[(String, Fingerprint)],
) -> Result<(), StoreError> {
    for (hash, fingerprint) in fingerprints {
        sqlx::query("INSERT OR IGNORE INTO perceptual (hash, dhash, phash) VALUES (?, ?, ?)")
            .bind(hash)
            .bind(fingerprint.dhash as i64)
            .bind(fingerprint.phash as i64)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub(super) async fn delete_fingerprint(pool: &SqlitePool, hash: &str) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM perceptual WHERE hash = ?")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) async fn library_fingerprints(
    pool: &SqlitePool,
    library: &str,
) -> Result<HashMap<String, Fingerprint>, StoreError> {
    let rows = sqlx::query(
        "SELECT p.hash AS hash, p.dhash AS dhash, p.phash AS phash
         FROM perceptual p
         INNER JOIN images i ON i.hash = p.hash
         WHERE i.library = ?",
    )
    .bind(library)
    .fetch_all(pool)
    .await?;
    let mut found = HashMap::new();
    for row in rows {
        found.insert(
            row.try_get::<String, _>("hash")?,
            Fingerprint {
                dhash: row.try_get::<i64, _>("dhash")? as u64,
                phash: row.try_get::<i64, _>("phash")? as u64,
            },
        );
    }
    Ok(found)
}

pub(super) async fn insert_images(
    pool: &SqlitePool,
    library: &str,
    images: &[&StagedImage],
) -> Result<(), StoreError> {
    let draw_count = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(draw_count) FROM images WHERE library = ?",
    )
    .bind(library)
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    for image in images {
        sqlx::query(
            "INSERT OR IGNORE INTO images (library, hash, size, draw_count) VALUES (?, ?, ?, ?)",
        )
        .bind(library)
        .bind(&image.hash)
        .bind(image.size as i64)
        .bind(draw_count)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub(super) async fn upsert_alias<'e, E>(
    executor: E,
    alias: &str,
    target: &str,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO aliases (alias, target) VALUES (?, ?)
         ON CONFLICT(alias) DO UPDATE SET target = excluded.target",
    )
    .bind(alias)
    .bind(target)
    .execute(executor)
    .await?;
    Ok(())
}

async fn library_min_draw(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    library: &str,
) -> Result<i64, StoreError> {
    let min = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(draw_count) FROM images WHERE library = ?",
    )
    .bind(library)
    .fetch_one(&mut **tx)
    .await?;
    Ok(min.unwrap_or(0))
}

pub(super) async fn merge_library(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    source: &str,
    dest: &str,
) -> Result<(), StoreError> {
    let source_min = library_min_draw(tx, source).await?;
    let dest_min = library_min_draw(tx, dest).await?;
    // 两边最小值对齐到同一基准，图保留相对本库最小值的偏移；重复图取较大偏移。
    let baseline = source_min.min(dest_min);
    sqlx::query("UPDATE images SET draw_count = ? + (draw_count - ?) WHERE library = ?")
        .bind(baseline)
        .bind(dest_min)
        .bind(dest)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO images (library, hash, size, draw_count)
         SELECT ?, hash, size, ? + (draw_count - ?) FROM images WHERE library = ?
         ON CONFLICT(library, hash) DO UPDATE SET
             draw_count = MAX(images.draw_count, excluded.draw_count)",
    )
    .bind(dest)
    .bind(baseline)
    .bind(source_min)
    .bind(source)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM images WHERE library = ?")
        .bind(source)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE aliases SET target = ? WHERE target = ?")
        .bind(dest)
        .bind(source)
        .execute(&mut **tx)
        .await?;
    upsert_alias(&mut **tx, source, dest).await?;
    Ok(())
}

pub(super) async fn prune_dangling_aliases(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM aliases WHERE target NOT IN (SELECT DISTINCT library FROM images)")
        .execute(pool)
        .await?;
    Ok(())
}
