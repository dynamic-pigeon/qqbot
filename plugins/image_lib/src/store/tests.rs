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

    store.wipe_library(group, "狗").await.unwrap();
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

    let wiped = store.wipe_library(group, "喵").await.unwrap();
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
async fn new_images_start_at_library_min_and_pick_increments() {
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
    let mut expected = vec![(sha256_hex(&a), 1), (sha256_hex(&b), 1)];
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
