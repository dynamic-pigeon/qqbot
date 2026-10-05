use super::backup::{RETAINED_DAYS, date_string};
use super::*;
use kovi::tokio;
use utils::sha256_hex;

fn temp_store() -> (Store, PathBuf) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("image_lib_store_{}_{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (
        Store::open_with_quota(
            dir.clone(),
            crate::config::DEFAULT_MAX_GROUP_MIB * 1024 * 1024,
        )
        .unwrap(),
        dir,
    )
}

fn png_like(tag: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG".to_vec();
    bytes.extend_from_slice(&[tag; 32]);
    bytes
}

/// 模拟下载管线：bytes 先落成 blobs 目录旁的临时文件再走正式入库。
async fn add_images(
    store: &Store,
    group_id: i64,
    name: &str,
    images: Vec<Vec<u8>>,
) -> Result<AddResult, StoreError> {
    let blobs = store.blobs_dir(group_id);
    std::fs::create_dir_all(&blobs).unwrap();
    let mut staged = Vec::with_capacity(images.len());
    for (index, bytes) in images.into_iter().enumerate() {
        let path = blobs.join(format!(".stage.test.{index}.tmp"));
        std::fs::write(&path, &bytes).unwrap();
        staged.push(StagedImage {
            hash: sha256_hex(&bytes),
            size: bytes.len() as u64,
            path,
        });
    }
    store.add_images(group_id, name, staged).await
}

#[tokio::test]
async fn adds_dedups_shares_blob_and_deletes() {
    let (store, dir) = temp_store();
    let group = 1;
    let a = png_like(1);
    let b = png_like(2);

    let first = add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    assert_eq!(
        first,
        AddResult {
            added: 1,
            skipped_dup: 0
        }
    );
    let again = add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    assert_eq!(
        again,
        AddResult {
            added: 0,
            skipped_dup: 1
        }
    );
    add_images(&store, group, "狗", vec![a.clone(), b.clone()])
        .await
        .unwrap();

    let blobs = std::fs::read_dir(dir.join("1").join("blobs"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .is_some_and(|e| e.path().extension().is_none())
        })
        .count();
    assert_eq!(blobs, 2);

    let hit = store.delete_hash(group, &sha256_hex(&a)).await.unwrap();
    let mut hit = hit;
    hit.sort();
    assert_eq!(hit, vec!["狗".to_owned(), "猫".to_owned()]);
    assert!(matches!(
        store.pick_random(group, "猫").await,
        Err(StoreError::LibraryMissing | StoreError::LibraryEmpty)
    ));
    assert!(store.pick_random(group, "狗").await.is_ok());
    assert_eq!(store.stats(group).await.unwrap().libraries.len(), 1);

    store.wipe_canonical_library(group, "狗").await.unwrap();
    assert!(store.stats(group).await.unwrap().libraries.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn alias_resolves_rejects_and_wipe_clears_canonical() {
    let (store, dir) = temp_store();
    let group = 2;
    add_images(&store, group, "猫", vec![png_like(1)])
        .await
        .unwrap();
    add_images(&store, group, "狗", vec![png_like(3)])
        .await
        .unwrap();
    let canonical = store.set_alias(group, "喵", "猫", false).await.unwrap();
    assert_eq!(canonical.canonical, "猫");
    assert_eq!(canonical.merged_from, None);
    assert_eq!(
        store
            .set_alias(group, "喵", "猫", false)
            .await
            .unwrap()
            .canonical,
        "猫"
    );
    assert!(store.pick_random(group, "喵").await.is_ok());
    assert!(matches!(
        store.set_alias(group, "喵", "狗", false).await,
        Err(StoreError::AliasTaken { alias, target })
            if alias == "喵" && target == "猫"
    ));
    assert!(store.pick_random(group, "喵").await.is_ok());
    assert!(matches!(
        store.set_alias(group, "狗", "猫", false).await,
        Err(StoreError::NameIsLibrary(_))
    ));
    assert!(matches!(
        store.set_alias(group, "龙", "不存在", false).await,
        Err(StoreError::TargetMissing(_))
    ));
    assert!(matches!(
        store.set_alias(group, "猫", "猫", true).await,
        Err(StoreError::AliasToSelf)
    ));

    add_images(&store, group, "喵", vec![png_like(2)])
        .await
        .unwrap();
    let stats = store.stats(group).await.unwrap();
    assert_eq!(stats.libraries.len(), 2);
    let cat = stats
        .libraries
        .iter()
        .find(|library| library.name == "猫")
        .unwrap();
    assert_eq!(cat.aliases, vec!["喵".to_owned()]);
    assert_eq!(cat.count, 2);

    // 生产路径是「登记时解析别名、确认时直清规范名」，这里两步模拟。
    let canonical = store.resolve_name(group, "喵").await.unwrap();
    let wiped = store
        .wipe_canonical_library(group, &canonical)
        .await
        .unwrap();
    assert_eq!(wiped, "猫");
    assert_eq!(store.stats(group).await.unwrap().libraries.len(), 1);
    assert!(matches!(
        store.pick_random(group, "喵").await,
        Err(StoreError::LibraryEmpty)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn merge_moves_images_and_keeps_old_name_as_alias() {
    let (store, dir) = temp_store();
    let group = 3;
    let shared = png_like(1);
    add_images(&store, group, "猫", vec![shared.clone(), png_like(2)])
        .await
        .unwrap();
    add_images(&store, group, "狗", vec![shared, png_like(3)])
        .await
        .unwrap();
    store.set_alias(group, "喵", "猫", false).await.unwrap();

    let merged = store.set_alias(group, "喵", "狗", true).await.unwrap();
    assert_eq!(merged.canonical, "狗");
    assert_eq!(merged.merged_from.as_deref(), Some("猫"));

    let stats = store.stats(group).await.unwrap();
    assert_eq!(stats.libraries.len(), 1);
    assert_eq!(stats.unique_count, 3);
    let dog = &stats.libraries[0];
    assert_eq!(dog.name, "狗");
    assert_eq!(dog.count, 3);
    assert_eq!(dog.aliases, vec!["喵".to_owned(), "猫".to_owned()]);

    assert!(store.pick_random(group, "猫").await.is_ok());
    assert!(store.pick_random(group, "喵").await.is_ok());
    add_images(&store, group, "猫", vec![png_like(4)])
        .await
        .unwrap();
    assert_eq!(store.stats(group).await.unwrap().libraries[0].count, 4);

    add_images(&store, group, "鸟", vec![png_like(5)])
        .await
        .unwrap();
    let merged = store.set_alias(group, "鸟", "狗", true).await.unwrap();
    assert_eq!(merged.merged_from.as_deref(), Some("鸟"));
    let stats = store.stats(group).await.unwrap();
    assert_eq!(stats.libraries.len(), 1);
    assert_eq!(stats.libraries[0].count, 5);
    assert!(store.pick_random(group, "鸟").await.is_ok());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn merge_aligns_draw_counts_to_the_lower_min() {
    let (store, dir) = temp_store();
    let group = 4;
    let shared = png_like(1);
    let only_cat = png_like(2);
    let only_dog = png_like(3);
    add_images(&store, group, "猫", vec![shared.clone(), only_cat.clone()])
        .await
        .unwrap();
    add_images(&store, group, "狗", vec![shared.clone(), only_dog.clone()])
        .await
        .unwrap();
    let shared_hash = sha256_hex(&shared);
    let cat_hash = sha256_hex(&only_cat);
    let dog_hash = sha256_hex(&only_dog);
    set_draw_count(&store, group, "猫", &shared_hash, 5).await;
    set_draw_count(&store, group, "猫", &cat_hash, 2).await;
    set_draw_count(&store, group, "狗", &shared_hash, 10).await;
    set_draw_count(&store, group, "狗", &dog_hash, 12).await;

    store.set_alias(group, "猫", "狗", true).await.unwrap();
    let mut got = draw_counts(&store, group, "狗").await.unwrap();
    got.sort();
    let mut expected = vec![(shared_hash, 5), (cat_hash, 2), (dog_hash, 4)];
    expected.sort();
    assert_eq!(got, expected);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn overview_resolves_alias_counts_and_rejects_missing() {
    let (store, dir) = temp_store();
    let group = 9;
    add_images(&store, group, "猫", vec![png_like(1), png_like(2)])
        .await
        .unwrap();
    store.set_alias(group, "喵", "猫", false).await.unwrap();

    let overview = store.library_overview(group, "喵").await.unwrap();
    assert_eq!(
        overview,
        LibraryOverview {
            canonical: "猫".to_owned(),
            count: 2,
            bytes: (png_like(1).len() + png_like(2).len()) as u64,
        }
    );

    assert!(matches!(
        store.library_overview(group, "不存在").await,
        Err(StoreError::LibraryMissing)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn canonical_wipe_ignores_alias_repointing_after_merge() {
    let (store, dir) = temp_store();
    let group = 10;
    add_images(&store, group, "猫", vec![png_like(1)])
        .await
        .unwrap();
    add_images(&store, group, "狗", vec![png_like(2)])
        .await
        .unwrap();
    // 登记待确认之后、执行之前，「别名 猫 狗 合并」把「猫」改成「狗」的别名。
    store.set_alias(group, "猫", "狗", true).await.unwrap();

    // 确认路径按规范名直清：「猫」已不是库，不得顺着别名清掉「狗」。
    assert!(matches!(
        store.wipe_canonical_library(group, "猫").await,
        Err(StoreError::LibraryMissing)
    ));
    let stats = store.stats(group).await.unwrap();
    assert_eq!(stats.libraries.len(), 1);
    assert_eq!(stats.libraries[0].name, "狗");
    assert_eq!(stats.libraries[0].count, 2);

    store.wipe_canonical_library(group, "狗").await.unwrap();
    assert!(store.stats(group).await.unwrap().libraries.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn rejects_when_group_quota_would_exceed() {
    let dir = std::env::temp_dir().join(format!("image_lib_quota_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open_with_quota(dir.clone(), 40).unwrap();
    add_images(&store, 7, "小", vec![png_like(3)])
        .await
        .unwrap();
    let err = add_images(&store, 7, "小", vec![png_like(4)])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::QuotaExceeded { .. }));

    let err = add_images(&store, 8, "小", vec![png_like(3), png_like(4)])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::QuotaExceeded { .. }));
    let leftover = std::fs::read_dir(dir.join("8").join("blobs"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_none())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(leftover, 0);
    let _ = std::fs::remove_dir_all(dir);
}

fn patterned_png(seed: u32) -> Vec<u8> {
    use image::{DynamicImage, Rgb, RgbImage};
    let image = RgbImage::from_fn(48, 48, |x, y| {
        let v = ((x.wrapping_mul(11) + y.wrapping_mul(5) + seed) % 256) as u8;
        Rgb([v, v.wrapping_add(30), 200u8.wrapping_sub(v)])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

#[test]
fn small_blob_stays_in_parallel_lane() {
    let dir = std::env::temp_dir().join("image_lib_head_probe");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("small.png");
    std::fs::write(&path, patterned_png(3)).unwrap();
    assert!(!is_large_blob(&path));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn fingerprints_cover_library_and_skip_undecodable() {
    let (store, dir) = temp_store();
    let group = 11;
    add_images(
        &store,
        group,
        "猫",
        vec![patterned_png(1), patterned_png(2)],
    )
    .await
    .unwrap();
    add_images(&store, group, "猫", vec![png_like(9)])
        .await
        .unwrap();

    let (canonical, images) = store.fingerprints_for_library(group, "猫").await.unwrap();
    assert_eq!(canonical, "猫");
    assert_eq!(images.len(), 2);

    store.set_alias(group, "喵", "猫", false).await.unwrap();
    let (alias, again) = store.fingerprints_for_library(group, "喵").await.unwrap();
    assert_eq!(alias, "猫");
    assert_eq!(again.len(), 2);
    // 落库断言：指纹必须持久化，否则每次查重都会全量重算。
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join(group.to_string()).join("index.db"))
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let stored = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM perceptual")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 2, "指纹未落库");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn hash_prefix_loads_deletes_and_rejects_bad_paths() {
    let (store, dir) = temp_store();
    let group = 21;
    let a = png_like(1);
    let b = png_like(2);
    add_images(&store, group, "猫", vec![a.clone(), b.clone()])
        .await
        .unwrap();
    add_images(&store, group, "狗", vec![a.clone()])
        .await
        .unwrap();

    let ha = sha256_hex(&a);
    let hb = sha256_hex(&b);
    assert_eq!(store.load_by_hash_prefix(group, &ha).await.unwrap(), a);

    let prefix = &ha[..12];
    assert!(
        !hb.starts_with(prefix),
        "fixture blobs unexpectedly share a 12-hex prefix"
    );
    assert_eq!(store.load_by_hash_prefix(group, prefix).await.unwrap(), a);
    assert_eq!(
        store
            .load_by_hash_prefix(group, &prefix.to_ascii_uppercase())
            .await
            .unwrap(),
        a
    );
    assert!(matches!(
        store
            .load_by_hash_prefix(group, "ffffffffffffffffffffffffffffffff")
            .await,
        Err(StoreError::ImageMissing)
    ));

    let libraries = store.delete_by_hash_prefix(group, prefix).await.unwrap();
    assert_eq!(libraries, vec!["狗".to_owned(), "猫".to_owned()]);
    assert!(matches!(
        store.load_by_hash_prefix(group, &ha).await,
        Err(StoreError::ImageMissing)
    ));
    assert_eq!(
        store.load_by_hash_prefix(group, &hb[..12]).await.unwrap(),
        b
    );

    assert!(store.read_blob(group, "../passwd").await.is_err());
    assert!(store.read_blob(group, "zz").await.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

async fn set_draw_count(store: &Store, group_id: i64, library: &str, hash: &str, count: i64) {
    store
        .with_group(group_id, |pool| async move {
            sqlx::query("UPDATE images SET draw_count = ? WHERE library = ? AND hash = ?")
                .bind(count)
                .bind(library)
                .bind(hash)
                .execute(&pool)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn draw_counts(
    store: &Store,
    group_id: i64,
    name: &str,
) -> Result<Vec<(String, i64)>, StoreError> {
    store
        .with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            let rows =
                sqlx::query("SELECT hash, draw_count FROM images WHERE library = ? ORDER BY hash")
                    .bind(&library)
                    .fetch_all(&pool)
                    .await?;
            rows.into_iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>("hash")?,
                        row.try_get::<i64, _>("draw_count")?,
                    ))
                })
                .collect()
        })
        .await
}

#[tokio::test]
async fn new_images_start_one_behind_library_min_and_pick_increments() {
    let (store, dir) = temp_store();
    let group = 31;
    let a = png_like(1);
    let b = png_like(2);
    add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    store.pick_random(group, "猫").await.unwrap();
    add_images(&store, group, "猫", vec![b.clone()])
        .await
        .unwrap();
    // a 抽过一次为 1，新图 b 按库内最小值 +1 入场为 2。
    let mut expected = vec![(sha256_hex(&a), 1), (sha256_hex(&b), 2)];
    expected.sort();
    assert_eq!(draw_counts(&store, group, "猫").await.unwrap(), expected);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn weight_halves_per_extra_draw() {
    let min = 3;
    assert_eq!(weight(min, min), 1 << 12);
    assert_eq!(weight(min + 1, min), 1 << 11);
    assert_eq!(weight(min + 12, min), 1);
    assert_eq!(weight(min + 13, min), 1);
}

/// 块状噪声图：与 similar.rs 的测试 fixture 同思路，块角能提出 SIFT 特征。
fn blocky_png(seed: u32) -> Vec<u8> {
    use image::{DynamicImage, Rgb, RgbImage};
    let control = |gx: u32, gy: u32, slot: u32| -> f64 {
        let h = seed
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(gx.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(gy.wrapping_mul(0xC2B2_AE35))
            .wrapping_add(slot.wrapping_mul(0x27D4_EB2F));
        ((h ^ (h >> 16)) % 1000) as f64 / 1000.0
    };
    let image = RgbImage::from_fn(512, 512, |x, y| {
        let (gx, gy) = (x / 16, y / 16);
        let fx = (x % 16) as f64 / 16.0;
        let fy = (y % 16) as f64 / 16.0;
        let base = control(gx, gy, 0) * 200.0;
        let slope = control(gx, gy, 1) * 40.0 - 20.0;
        let detail = ((x.wrapping_mul(31) ^ y.wrapping_mul(17)) % 16) as f64 / 16.0;
        let v = (base + slope * (fx - fy) + detail * 12.0 + 12.0).clamp(0.0, 255.0) as u8;
        Rgb([v, v / 2 + 40, 220u8.saturating_sub(v / 3)])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// 与 similar.rs 的 photo_like 同思路：块角是强角点、各角邻域梯度组合互不
/// 相同，中心裁剪能被 SIFT 检出；纯块状噪声的角点描述子彼此同构，检不出。
fn photo_like_jpeg(seed: u32, keep_percent: Option<u32>) -> Vec<u8> {
    use image::{ImageEncoder, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
    let control = |gx: u32, gy: u32, slot: u32| -> f64 {
        let h = seed
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(gx.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(gy.wrapping_mul(0xC2B2_AE35))
            .wrapping_add(slot.wrapping_mul(0x27D4_EB2F));
        ((h ^ (h >> 16)) % 1000) as f64 / 1000.0
    };
    let full = RgbImage::from_fn(512, 512, |x, y| {
        let (gx, gy) = (x / 16, y / 16);
        let fx = (x % 16) as f64 / 16.0;
        let fy = (y % 16) as f64 / 16.0;
        let base = control(gx, gy, 0) * 200.0;
        let slope = control(gx, gy, 1) * 40.0 - 20.0;
        let detail = ((x.wrapping_mul(31) ^ y.wrapping_mul(17)) % 16) as f64 / 16.0;
        let v = (base + slope * (fx - fy) + detail * 12.0 + 12.0).clamp(0.0, 255.0) as u8;
        Rgb([v, v / 2 + 40, 220u8.saturating_sub(v / 3)])
    });
    let image = match keep_percent {
        None => full,
        Some(keep) => {
            let side = 512 * keep / 100;
            let offset = (512 - side) / 2;
            image::imageops::crop_imm(&full, offset, offset, side, side).to_image()
        }
    };
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, 85)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    buf
}

async fn table_count(dir: &std::path::Path, group: i64, table: &str) -> i64 {
    // sqlx 0.9 的 query_scalar 只收 'static str，表名映射成字面量。
    let sql: &'static str = match table {
        "crop_pairs" => "SELECT COUNT(*) FROM crop_pairs",
        "crop_scan_state" => "SELECT COUNT(*) FROM crop_scan_state",
        _ => unreachable!("仅用于配对缓存两张表"),
    };
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join(group.to_string()).join("index.db"))
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let count = sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}

#[tokio::test]
async fn crop_scan_persists_pairs_and_repeats_from_cache() {
    let (store, dir) = temp_store();
    let group = 61;
    let whole = photo_like_jpeg(3, None);
    let part = photo_like_jpeg(3, Some(40));
    add_images(&store, group, "猫", vec![whole.clone(), part.clone()])
        .await
        .unwrap();

    // 首扫：增量计划（还没有任何覆盖），算出 1 对并落库。
    let (canonical, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    assert_eq!(canonical, "猫");
    assert!(matches!(
        plan,
        CropPlan::Incremental {
            had_coverage: false,
            ..
        }
    ));
    let groups = store.crop_scan_run(group, plan).await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0].hashes,
        vec![sha256_hex(&whole), sha256_hex(&part)]
    );
    assert_eq!(table_count(&dir, group, "crop_pairs").await, 1);
    assert_eq!(table_count(&dir, group, "crop_scan_state").await, 1);

    // 重扫：覆盖完整，直接从缓存拼出同一份结果。
    let (_, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    let cached = match plan {
        CropPlan::Complete(groups) => groups,
        other => panic!("应全命中: {other:?}"),
    };
    assert_eq!(cached, groups);

    // 新增一张无关图：只补算带新端点的对，无新增正结果。
    add_images(&store, group, "猫", vec![photo_like_jpeg(9, None)])
        .await
        .unwrap();
    let (_, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    assert!(matches!(
        plan,
        CropPlan::Incremental {
            had_coverage: true,
            ..
        }
    ));
    let again = store.crop_scan_run(group, plan).await.unwrap();
    assert_eq!(again, groups);
    assert_eq!(table_count(&dir, group, "crop_pairs").await, 1);

    // 删掉局部图：正结果对与覆盖标记随之作废，该库回到增量重算。
    store.delete_hash(group, &sha256_hex(&part)).await.unwrap();
    assert_eq!(table_count(&dir, group, "crop_pairs").await, 0);
    assert_eq!(table_count(&dir, group, "crop_scan_state").await, 0);
    let (_, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    assert!(matches!(plan, CropPlan::Incremental { .. }));
    assert!(store.crop_scan_run(group, plan).await.unwrap().is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn crop_cache_is_wiped_when_invalidation_marker_changes() {
    let (store, dir) = temp_store();
    let group = 62;
    let whole = photo_like_jpeg(3, None);
    let part = photo_like_jpeg(3, Some(40));
    add_images(&store, group, "猫", vec![whole, part])
        .await
        .unwrap();
    let (_, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    store.crop_scan_run(group, plan).await.unwrap();
    assert_eq!(table_count(&dir, group, "crop_pairs").await, 1);

    // 篡改失效标记，模拟「换阈值或换算法版本后留下的旧账」。
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join(group.to_string()).join("index.db"));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("UPDATE schema_meta SET value = 'v9:8' WHERE key = 'crop_cache'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // 新开的 Store 会在 ensure_pool → init_schema 里按标记弃账。
    let reopened = Store::open_with_quota(dir.clone(), u64::MAX).unwrap();
    reopened.stats(group).await.unwrap();
    assert_eq!(table_count(&dir, group, "crop_pairs").await, 0);
    assert_eq!(table_count(&dir, group, "crop_scan_state").await, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sifts_cover_library_and_persist() {
    let (store, dir) = temp_store();
    let group = 17;
    add_images(&store, group, "猫", vec![blocky_png(3), blocky_png(7)])
        .await
        .unwrap();
    add_images(&store, group, "猫", vec![png_like(9)])
        .await
        .unwrap();

    let (canonical, plan) = store.crop_scan_prepare(group, "猫").await.unwrap();
    assert_eq!(canonical, "猫");
    let images = match plan {
        CropPlan::Incremental { images, .. } => images,
        other => panic!("无覆盖时应走增量: {other:?}"),
    };
    // 无纹理的假 PNG 提不出指纹，与查重口径一致地跳过。
    assert_eq!(images.len(), 2);
    assert!(images.iter().all(|image| !image.sift.points.is_empty()));

    // 落库断言：特征必须持久化，否则每次查裁剪都会全库重算。
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join(group.to_string()).join("index.db"))
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let stored = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sift")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 2, "SIFT 特征未落库");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn narrow_sift_table_is_rebuilt_on_open() {
    let (store, dir) = temp_store();
    let group = 87;
    let db = dir.join(group.to_string()).join("index.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE sift (
            hash TEXT NOT NULL PRIMARY KEY CHECK (length(hash) = 64),
            features BLOB NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    // 任意一次库操作都会走 ensure_pool → init_schema，触发换代重建。
    store.stats(group).await.err();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .read_only(true);
    let check = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let sql = sqlx::query_scalar::<_, String>("SELECT sql FROM sqlite_master WHERE name = 'sift'")
        .fetch_one(&check)
        .await
        .unwrap();
    assert!(sql.contains("% 132 = 0"), "narrow table not rebuilt: {sql}");
    let version =
        sqlx::query_scalar::<_, String>("SELECT value FROM schema_meta WHERE key = 'sift'")
            .fetch_one(&check)
            .await
            .unwrap();
    assert_eq!(version, "v1");
    check.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn newer_sift_schema_is_rejected() {
    let (store, dir) = temp_store();
    let group = 88;
    let db = dir.join(group.to_string()).join("index.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE schema_meta (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schema_meta VALUES ('sift', 'v9')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let result = store.stats(group).await;
    assert!(result.is_err(), "新版本库不应被旧代码打开");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn narrow_fingerprint_table_is_rebuilt_on_open() {
    let (store, dir) = temp_store();
    let group = 77;
    let db = dir.join(group.to_string()).join("index.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE perceptual (
            hash TEXT NOT NULL PRIMARY KEY CHECK (length(hash) = 64),
            dhash BLOB NOT NULL CHECK (length(dhash) = 16),
            phash BLOB NOT NULL CHECK (length(phash) = 16)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    // 任意一次库操作都会走 ensure_pool → init_schema，触发换代重建。
    store.stats(group).await.err();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .read_only(true);
    let check = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let sql =
        sqlx::query_scalar::<_, String>("SELECT sql FROM sqlite_master WHERE name = 'perceptual'")
            .fetch_one(&check)
            .await
            .unwrap();
    assert!(
        sql.contains("length(dhash) = 32"),
        "narrow table not rebuilt: {sql}"
    );
    let version =
        sqlx::query_scalar::<_, String>("SELECT value FROM schema_meta WHERE key = 'perceptual'")
            .fetch_one(&check)
            .await
            .unwrap();
    assert_eq!(version, "v2");
    check.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn newer_fingerprint_schema_is_rejected() {
    let (store, dir) = temp_store();
    let group = 78;
    let db = dir.join(group.to_string()).join("index.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE schema_meta (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schema_meta VALUES ('perceptual', 'v9')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let result = store.stats(group).await;
    assert!(result.is_err(), "新版本库不应被旧代码打开");
    let _ = std::fs::remove_dir_all(dir);
}

/// 备份目录下唯一的日期目录;没有则 panic。
fn only_date_dir(backups: &PathBuf) -> PathBuf {
    std::fs::read_dir(backups)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("备份目录里应有日期目录")
}

#[tokio::test]
async fn deleted_blob_survives_protection_window_then_is_reclaimed() {
    let (store, dir) = temp_store();
    let group = 41;
    let a = png_like(1);
    let b = png_like(2);
    add_images(&store, group, "猫", vec![a.clone(), b.clone()])
        .await
        .unwrap();

    let day = 20_725;
    store.backup_daily_at(day).await;
    let group_backup = dir
        .join("backups")
        .join(date_string(day))
        .join(group.to_string());
    assert!(group_backup.join("index.db").is_file());
    let hash = sha256_hex(&a);
    let live_blob = dir.join(group.to_string()).join("blobs").join(&hash);

    // 删除只清索引行,文件留在原位等对账回收。
    store.delete_hash(group, &hash).await.unwrap();
    assert_eq!(std::fs::read(&live_blob).unwrap(), a);

    // 保护期内对账不回收,快照 db 保留备份时刻的索引。
    store.reconcile_all_at(day + 3).await;
    assert!(live_blob.is_file(), "备份还指向的图不得回收");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(group_backup.join("index.db"))
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let rows = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM images")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2, "备份快照应保留备份时刻的索引");
    pool.close().await;

    // 过了保护期,对账把孤儿文件连同过期记录一起收走。
    store.reconcile_all_at(day + RETAINED_DAYS as i64).await;
    assert!(!live_blob.exists(), "保护期外孤儿文件应被回收");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn unbacked_orphans_are_reclaimed_immediately() {
    let (store, dir) = temp_store();
    let group = 45;
    let a = png_like(1);
    add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    let hash = sha256_hex(&a);
    let live_blob = dir.join(group.to_string()).join("blobs").join(&hash);

    // 从没被任何备份指向过的图,删除后没有保护期。
    store.delete_hash(group, &hash).await.unwrap();
    store.reconcile_all_at(20_000).await;
    assert!(!live_blob.exists(), "未被备份指向的孤儿应立即回收");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn readding_image_within_window_reuses_retained_blob() {
    let (store, dir) = temp_store();
    let group = 46;
    let a = png_like(1);
    add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    let day = 20_000;
    store.backup_daily_at(day).await;
    let hash = sha256_hex(&a);

    store.delete_hash(group, &hash).await.unwrap();
    // 保护期内重新入库:add 复用原位的文件,不该再走一次落盘。
    let result = add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    assert_eq!(
        result,
        AddResult {
            added: 1,
            skipped_dup: 0
        }
    );
    assert_eq!(store.read_blob(group, &hash).await.unwrap(), a);

    // 重新入库让下一次备份刷新记录,保护期从新备份日重新起算。
    store.backup_daily_at(day + 1).await;
    store.delete_hash(group, &hash).await.unwrap();
    store.reconcile_all_at(day + 6).await;
    assert!(
        store.blobs_dir(group).join(&hash).is_file(),
        "续期后的保护记录仍应保住文件"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn same_day_backup_is_skipped_and_stale_tmp_cleaned() {
    let (store, dir) = temp_store();
    add_images(&store, 43, "猫", vec![png_like(1)])
        .await
        .unwrap();
    let backups = dir.join("backups");

    store.backup_daily_at(20_000).await;
    let day = only_date_dir(&backups);
    // 哨兵文件:同日重跑若重建备份目录,哨兵会消失。
    std::fs::write(day.join("marker"), b"x").unwrap();

    store.backup_daily_at(20_000).await;
    assert!(day.join("marker").is_file(), "同日重跑不得重建备份目录");

    // 崩溃留下的 <date>.tmp 残留在下次运行时清掉。
    let stale = backups.join("1999-01-01.tmp");
    std::fs::create_dir_all(&stale).unwrap();
    store.backup_daily_at(20_001).await;
    assert!(!stale.exists(), "半成品备份残留应被清理");
    assert_eq!(
        std::fs::read_dir(&backups)
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        2,
        "应只剩两个日期目录"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn backup_rolls_to_last_seven_days_and_keeps_foreign_dirs() {
    let (store, dir) = temp_store();
    add_images(&store, 44, "猫", vec![png_like(1)])
        .await
        .unwrap();
    let backups = dir.join("backups");
    // 人工放进去的目录不属于滚动管理,不得误删。
    std::fs::create_dir_all(backups.join("manual-copy")).unwrap();

    for offset in 0..9 {
        store.backup_daily_at(20_000 + offset).await;
    }

    let mut dates: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "manual-copy")
        .collect();
    dates.sort();
    let expected: Vec<String> = (2..=8).map(|offset| date_string(20_000 + offset)).collect();
    assert_eq!(dates, expected, "应只保留最近七天的日期目录");
    assert!(backups.join("manual-copy").is_dir());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn wiped_library_keeps_files_until_protection_expires() {
    let (store, dir) = temp_store();
    let group = 47;
    let a = png_like(1);
    add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    let day = 20_000;
    store.backup_daily_at(day).await;
    let hash = sha256_hex(&a);
    let live_blob = dir.join(group.to_string()).join("blobs").join(&hash);

    store.wipe_canonical_library(group, "猫").await.unwrap();
    assert!(live_blob.is_file(), "清库不得物理删除文件");

    store.reconcile_all_at(day + 3).await;
    assert!(live_blob.is_file(), "保护期内对账不得回收清库后的文件");
    store.reconcile_all_at(day + RETAINED_DAYS as i64).await;
    assert!(!live_blob.exists(), "保护期外文件应被回收");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn backup_covers_all_groups_and_skips_broken_one() {
    let (store, dir) = temp_store();
    add_images(&store, 48, "猫", vec![png_like(1)])
        .await
        .unwrap();
    // 群 49 的 index.db 是坏文件,备份应跳过它而不拖垮整份备份。
    let broken = dir.join("49");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("index.db"), b"not a sqlite db").unwrap();

    store.backup_daily_at(20_000).await;
    let day = dir.join("backups").join(date_string(20_000));
    assert!(day.join("48").join("index.db").is_file(), "正常群应进备份");
    assert!(!day.join("49").exists(), "坏群的半成品不得进正式备份");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn snapshot_carries_previous_day_refs() {
    let (store, dir) = temp_store();
    let group = 50;
    add_images(&store, group, "猫", vec![png_like(1)])
        .await
        .unwrap();
    store.backup_daily_at(20_000).await;
    add_images(&store, group, "猫", vec![png_like(2)])
        .await
        .unwrap();
    store.backup_daily_at(20_001).await;

    // 刷 refs 在快照落成之后,快照里带到的是截至上次备份日的表:D0
    // 后新增的图不在其中,只有 D0 时已在库里的图带着 D0 的记录。顺带
    // 回归这条顺序——refs 指向今天,当且仅当今天的快照已经存在。
    let snap = dir
        .join("backups")
        .join(date_string(20_001))
        .join(group.to_string())
        .join("index.db");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&snap)
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let row = sqlx::query("SELECT COUNT(*) AS n, MAX(last_backup_day) AS d FROM backup_refs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<i64, _>("n").unwrap(), 1);
    assert_eq!(row.try_get::<Option<i64>, _>("d").unwrap(), Some(20_000));
    pool.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn backup_gap_keeps_directory_and_protection_in_lockstep() {
    let (store, dir) = temp_store();
    let group = 51;
    let a = png_like(1);
    add_images(&store, group, "猫", vec![a.clone()])
        .await
        .unwrap();
    let hash = sha256_hex(&a);
    let live_blob = dir.join(group.to_string()).join("blobs").join(&hash);
    // 真实顺序:备份过,然后删除,随后停机。
    store.backup_daily_at(20_000).await;
    store.delete_hash(group, &hash).await.unwrap();
    store.backup_daily_at(20_006).await;
    let backups = dir.join("backups");
    assert!(backups.join(date_string(20_000)).is_dir());
    store.reconcile_all_at(20_006).await;
    assert!(live_blob.is_file(), "D0 目录还在,其指向的图不得被回收");

    // D7:过期目录与保护记录同日失效,「目录在 ⟺ 数据在」保持一致。
    store.backup_daily_at(20_007).await;
    store.reconcile_all_at(20_007).await;
    assert!(
        !backups.join(date_string(20_000)).exists(),
        "过期的备份目录应被滚动删除"
    );
    assert!(!live_blob.exists(), "记录过期后文件应被回收");
    let _ = std::fs::remove_dir_all(dir);
}
