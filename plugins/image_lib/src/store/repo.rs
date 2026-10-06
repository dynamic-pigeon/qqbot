//! 图库 SQL 层：所有直接面对 sqlite 的查询与变更。

use std::collections::{HashMap, HashSet};

use sqlx::{Row, SqlitePool};

use super::{StagedImage, StoreError};
use crate::similar::{
    FINGERPRINT_WORDS, Fingerprint, SiftFeatures, sift_from_bytes, sift_to_bytes,
};

/// 指纹词组序列化成大端 BLOB：4×u64 = 32 字节，与建表 CHECK 对齐。
fn pack_words(words: &[u64; FINGERPRINT_WORDS]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_be_bytes()).collect()
}

fn unpack_words(bytes: &[u8]) -> Option<[u64; FINGERPRINT_WORDS]> {
    let mut words = [0u64; FINGERPRINT_WORDS];
    for (i, word) in words.iter_mut().enumerate() {
        let start = i * 8;
        *word = u64::from_be_bytes(bytes.get(start..start + 8)?.try_into().ok()?);
    }
    Some(words)
}

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
        // 只忽略主键冲突（并发补指纹的幂等）；CHECK 违约必须报错，
        // 不能像 OR IGNORE 那样把约束错误也静默吞掉。
        sqlx::query(
            "INSERT INTO perceptual (hash, dhash, phash) VALUES (?, ?, ?)
             ON CONFLICT(hash) DO NOTHING",
        )
        .bind(hash)
        .bind(pack_words(&fingerprint.dhash))
        .bind(pack_words(&fingerprint.phash))
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
        let hash = row.try_get::<String, _>("hash")?;
        let dhash = unpack_words(&row.try_get::<Vec<u8>, _>("dhash")?)
            .ok_or_else(|| corrupt_fingerprint(&hash))?;
        let phash = unpack_words(&row.try_get::<Vec<u8>, _>("phash")?)
            .ok_or_else(|| corrupt_fingerprint(&hash))?;
        found.insert(hash, Fingerprint { dhash, phash });
    }
    Ok(found)
}

fn corrupt_fingerprint(hash: &str) -> StoreError {
    StoreError::Other(anyhow::anyhow!("指纹 BLOB 长度异常: {hash}"))
}

pub(super) async fn insert_sifts(
    pool: &SqlitePool,
    features: &[(String, SiftFeatures)],
) -> Result<(), StoreError> {
    for (hash, features) in features {
        // 与指纹同款：只忽略主键冲突（并发补特征的幂等）。
        sqlx::query(
            "INSERT INTO sift (hash, features) VALUES (?, ?)
             ON CONFLICT(hash) DO NOTHING",
        )
        .bind(hash)
        .bind(sift_to_bytes(features))
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub(super) async fn delete_sift(pool: &SqlitePool, hash: &str) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM sift WHERE hash = ?")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) async fn library_sifts(
    pool: &SqlitePool,
    library: &str,
) -> Result<HashMap<String, SiftFeatures>, StoreError> {
    let rows = sqlx::query(
        "SELECT s.hash AS hash, s.features AS features
         FROM sift s
         INNER JOIN images i ON i.hash = s.hash
         WHERE i.library = ?",
    )
    .bind(library)
    .fetch_all(pool)
    .await?;
    let mut found = HashMap::new();
    for row in rows {
        let hash = row.try_get::<String, _>("hash")?;
        let bytes = row.try_get::<Vec<u8>, _>("features")?;
        let features = sift_from_bytes(&bytes)
            .ok_or_else(|| StoreError::Other(anyhow::anyhow!("SIFT BLOB 异常: {hash}")))?;
        found.insert(hash, features);
    }
    Ok(found)
}

/// 读一个库的覆盖标记：covered 是 64 位 hex 哈希直接拼接，定长切块还原。
pub(super) async fn crop_covered(
    pool: &SqlitePool,
    library: &str,
) -> Result<Vec<String>, StoreError> {
    let covered =
        sqlx::query_scalar::<_, String>("SELECT covered FROM crop_scan_state WHERE library = ?")
            .bind(library)
            .fetch_optional(pool)
            .await?;
    let Some(covered) = covered else {
        return Ok(Vec::new());
    };
    covered
        .as_bytes()
        .chunks(64)
        .map(|chunk| {
            std::str::from_utf8(chunk)
                .map(str::to_owned)
                .map_err(|_| StoreError::Other(anyhow::anyhow!("覆盖标记 BLOB 异常: {library}")))
        })
        .collect()
}

/// 整轮扫描完成后写入覆盖标记：本轮成员全集即「已两两比对过」的范围。
pub(super) async fn save_crop_covered(
    pool: &SqlitePool,
    library: &str,
    covered: &[String],
) -> Result<(), StoreError> {
    let mut blob = String::with_capacity(covered.len() * 64);
    for hash in covered {
        blob.push_str(hash);
    }
    sqlx::query(
        "INSERT INTO crop_scan_state (library, covered) VALUES (?, ?)
         ON CONFLICT(library) DO UPDATE SET covered = excluded.covered",
    )
    .bind(library)
    .bind(blob)
    .execute(pool)
    .await?;
    Ok(())
}

/// 新查出的正结果对落库。只忽略主键冲突（并发扫描的幂等），与指纹同款。
pub(super) async fn insert_crop_pairs(
    pool: &SqlitePool,
    pairs: &[(String, String, u8)],
) -> Result<(), StoreError> {
    if pairs.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (whole, part, percent) in pairs {
        sqlx::query(
            "INSERT INTO crop_pairs (whole, part, percent) VALUES (?, ?, ?)
             ON CONFLICT(whole, part) DO NOTHING",
        )
        .bind(whole)
        .bind(part)
        .bind(u64::from(*percent) as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 该库成员之间的全部正结果对。EXISTS 子查询借 images 主键索引过滤，
/// 已删出的哈希自然不在结果里；对的结果只依赖两张图的内容，跨库共享。
pub(super) async fn library_crop_pairs(
    pool: &SqlitePool,
    library: &str,
) -> Result<Vec<(String, String, u8)>, StoreError> {
    let rows = sqlx::query(
        "SELECT c.whole AS whole, c.part AS part, c.percent AS percent
         FROM crop_pairs c
         WHERE EXISTS (SELECT 1 FROM images i WHERE i.library = ? AND i.hash = c.whole)
           AND EXISTS (SELECT 1 FROM images i WHERE i.library = ? AND i.hash = c.part)",
    )
    .bind(library)
    .bind(library)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("whole")?,
                row.try_get::<String, _>("part")?,
                row.try_get::<i64, _>("percent")? as u8,
            ))
        })
        .collect()
}

pub(super) async fn delete_crop_state<'e, E>(executor: E, library: &str) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM crop_scan_state WHERE library = ?")
        .bind(library)
        .execute(executor)
        .await?;
    Ok(())
}

/// blob 物理回收时清掉该哈希参与的正结果对（在对账事务里调用）。
pub(super) async fn delete_crop_pairs_for_hash<'e, E>(
    executor: E,
    hash: &str,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM crop_pairs WHERE whole = ? OR part = ?")
        .bind(hash)
        .bind(hash)
        .execute(executor)
        .await?;
    Ok(())
}

/// 把哈希从所有覆盖标记里摘除（重写 covered 去掉那个 64 字符槽位，摘空
/// 删行），与正结果对的回收在同一事务。不摘的话，「blob 已回收 → 同内容
/// 图再加回」会对着已删的正结果行被当成已比对而漏检。哈希与槽位同为
/// 64 字节、按槽对齐拼接，instr 只可能整槽命中，不会误伤相邻哈希。
pub(super) async fn shrink_hash_from_covered(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hash: &str,
) -> Result<(), StoreError> {
    let rows =
        sqlx::query("SELECT library, covered FROM crop_scan_state WHERE instr(covered, ?) > 0")
            .bind(hash)
            .fetch_all(&mut **tx)
            .await?;
    for row in rows {
        let library: String = row.try_get("library")?;
        let covered: String = row.try_get("covered")?;
        let shrunk: String = covered
            .as_bytes()
            .chunks(64)
            .filter(|slot| *slot != hash.as_bytes())
            .map(|slot| std::str::from_utf8(slot).map(str::to_owned))
            .collect::<Result<String, _>>()
            .map_err(|_| StoreError::Other(anyhow::anyhow!("覆盖标记异常: {library}")))?;
        if shrunk.is_empty() {
            sqlx::query("DELETE FROM crop_scan_state WHERE library = ?")
                .bind(&library)
                .execute(&mut **tx)
                .await?;
        } else {
            sqlx::query("UPDATE crop_scan_state SET covered = ? WHERE library = ?")
                .bind(&shrunk)
                .bind(&library)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

pub(super) async fn insert_images(
    pool: &SqlitePool,
    library: &str,
    images: &[&StagedImage],
) -> Result<(), StoreError> {
    // 新图按「落后一次」入场（库内最小值 +1），垫底的老图优先于新图。
    // 空库时 MIN 为 NULL，+1 后仍是 NULL，unwrap_or 兜底为 0。
    let draw_count = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(draw_count) + 1 FROM images WHERE library = ?",
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
    // 源库的覆盖标记随成员一起并走：目标库的标记不含并进来的图，
    // 下次查裁剪会按增量补算它们的对。
    delete_crop_state(&mut **tx, source).await?;
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

/// 备份时刷新保护记录：索引里现存的每个 hash 都被今天的备份指向。
/// SELECT 带上 WHERE true:不带 WHERE 的 INSERT...SELECT 直接跟 ON
/// CONFLICT 会被 SQLite 当成语法错误(解析歧义)。
pub(super) async fn refresh_backup_refs(pool: &SqlitePool, day: i64) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO backup_refs (hash, last_backup_day)
         SELECT DISTINCT hash, ? FROM images WHERE true
         ON CONFLICT(hash) DO UPDATE SET last_backup_day = excluded.last_backup_day",
    )
    .bind(day)
    .execute(pool)
    .await?;
    Ok(())
}

/// 清掉已过保护期的记录。先清再判定，之后表里剩的都是仍在保护期内的引用。
pub(super) async fn purge_expired_backup_refs(
    pool: &SqlitePool,
    retain_days: i64,
    today: i64,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM backup_refs WHERE last_backup_day + ? <= ?")
        .bind(retain_days)
        .bind(today)
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) async fn backed_up_hashes(pool: &SqlitePool) -> Result<HashSet<String>, StoreError> {
    let hashes = sqlx::query_scalar::<_, String>("SELECT hash FROM backup_refs")
        .fetch_all(pool)
        .await?;
    Ok(hashes.into_iter().collect())
}
