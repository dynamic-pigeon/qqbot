//! 查重与查裁剪的读路径：锁内快照，锁外解码，再写回。
//! 查重只补指纹；查裁剪同一次解码写指纹和 SIFT，两条写回不合成一个泛型。

use std::{collections::HashSet, path::PathBuf};

use super::blob_fs::blob_file;
use super::derive::derive_missing;
use super::repo::{
    crop_covered, insert_crop_pairs, insert_fingerprints, insert_sifts, library_crop_pairs,
    library_exists, library_fingerprints, library_hashes, library_sifts, resolve_library,
    save_crop_covered,
};
use super::{CropPlan, Store, StoreError};
use crate::similar::{
    HashedImage, SiftableImage, SimilarGroup, assemble_crop_groups, crop_group, detect_crops,
    fingerprint_and_sift, fingerprint_bytes,
};

impl Store {
    /// 解析库名（含别名），补齐缺失的感知哈希后返回可比较的图。
    /// 重算缺失指纹是读盘加解码的重活：锁内只做快照与写回，解码在群锁外
    /// 并行进行，首查重全库重算期间同群的抽图/加图不会被卡住。
    pub async fn fingerprints_for_library(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<(String, Vec<HashedImage>), StoreError> {
        let blobs = self.blobs_dir(group_id);
        let (library, hashes, mut fingerprints, missing) = self
            .with_group(group_id, |pool| async move {
                let library = resolve_library(&pool, name).await?;
                if !library_exists(&pool, &library).await? {
                    return Err(StoreError::LibraryMissing);
                }
                let hashes: Vec<String> =
                    library_hashes(&pool, &library).await?.into_iter().collect();
                let fingerprints = library_fingerprints(&pool, &library).await?;
                // 锁外算指纹期间可能有并发删除，算完的孤儿行由对账任务清理。
                let missing: Vec<(String, PathBuf)> = hashes
                    .iter()
                    .filter(|hash| !fingerprints.contains_key(*hash))
                    .filter_map(|hash| {
                        blob_file(&blobs, hash)
                            .ok()
                            .map(|path| (hash.clone(), path))
                    })
                    .collect();
                Ok((library, hashes, fingerprints, missing))
            })
            .await?;

        let computed = derive_missing(missing, fingerprint_bytes).await?;

        if !computed.is_empty() {
            let to_write = &computed;
            self.with_group(group_id, |pool| async move {
                insert_fingerprints(&pool, to_write).await
            })
            .await?;
        }
        for (hash, fingerprint) in computed {
            fingerprints.insert(hash, fingerprint);
        }

        let images = hashes
            .into_iter()
            .filter_map(|hash| {
                fingerprints
                    .remove(&hash)
                    .map(|fingerprint| HashedImage { hash, fingerprint })
            })
            .collect();
        Ok((library, images))
    }

    /// 按规范库名补齐指纹与 SIFT 特征：两张表共用一次解码
    /// （[`fingerprint_and_sift`]），任一缺失的图都会重算——存量库首次
    /// 查裁剪等于全库重算一遍，与首查重同量级。锁外解码期间同群的
    /// 抽图/加图不会被卡住（同上）。[`crop_scan_prepare`] 在锁内判完缓存
    /// 覆盖才走到这里，规范名直传，避免两段锁之间别名被并发改指。
    async fn siftables_for_canonical(
        &self,
        group_id: i64,
        library: &str,
    ) -> Result<Vec<SiftableImage>, StoreError> {
        let blobs = self.blobs_dir(group_id);
        let (hashes, mut fingerprints, mut sifts, missing) = self
            .with_group(group_id, |pool| async move {
                let hashes: Vec<String> =
                    library_hashes(&pool, library).await?.into_iter().collect();
                let fingerprints = library_fingerprints(&pool, library).await?;
                let sifts = library_sifts(&pool, library).await?;
                let missing: Vec<(String, PathBuf)> = hashes
                    .iter()
                    .filter(|hash| !fingerprints.contains_key(*hash) || !sifts.contains_key(*hash))
                    .filter_map(|hash| {
                        blob_file(&blobs, hash)
                            .ok()
                            .map(|path| (hash.clone(), path))
                    })
                    .collect();
                Ok((hashes, fingerprints, sifts, missing))
            })
            .await?;

        let computed = derive_missing(missing, fingerprint_and_sift).await?;

        if !computed.is_empty() {
            let to_fingerprints: Vec<_> = computed
                .iter()
                .map(|(hash, (fingerprint, _))| (hash.clone(), *fingerprint))
                .collect();
            let to_sifts: Vec<_> = computed
                .iter()
                .map(|(hash, (_, sift))| (hash.clone(), sift.clone()))
                .collect();
            self.with_group(group_id, |pool| {
                let to_fingerprints = &to_fingerprints;
                let to_sifts = &to_sifts;
                async move {
                    insert_fingerprints(&pool, to_fingerprints).await?;
                    insert_sifts(&pool, to_sifts).await
                }
            })
            .await?;
            for (hash, (fingerprint, sift)) in computed {
                fingerprints.insert(hash.clone(), fingerprint);
                sifts.insert(hash, sift);
            }
        }

        let images = hashes
            .into_iter()
            .filter_map(|hash| {
                let fingerprint = fingerprints.remove(&hash)?;
                let sift = sifts.remove(&hash)?;
                Some(SiftableImage {
                    hash,
                    fingerprint,
                    sift,
                })
            })
            .collect();
        Ok(images)
    }

    /// 「查裁剪」的缓存判定：覆盖集（最近一次完整扫描的成员全集）包含全部
    /// 成员时，直接从正结果缓存拼出结果；否则给出增量计划，只补算有新
    /// 端点的对。blob 按内容寻址不可变，算过的对永远有效：逻辑删图与清库
    /// 零成本（被删哈希由结果重建的 EXISTS 过滤），覆盖集只在 blob 被
    /// 对账物理回收时收缩、换阈值/算法版本时整体作废。
    pub async fn crop_scan_prepare(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<(String, CropPlan), StoreError> {
        let (library, members, covered, cached) = self
            .with_group(group_id, |pool| async move {
                let library = resolve_library(&pool, name).await?;
                if !library_exists(&pool, &library).await? {
                    return Err(StoreError::LibraryMissing);
                }
                let mut members: Vec<String> =
                    library_hashes(&pool, &library).await?.into_iter().collect();
                members.sort();
                let covered = crop_covered(&pool, &library).await?;
                if members.len() < 2 || members.iter().all(|hash| covered.contains(hash)) {
                    let pairs = library_crop_pairs(&pool, &library).await?;
                    let groups = assemble_crop_groups(
                        pairs
                            .into_iter()
                            .map(|(whole, part, percent)| crop_group(&whole, &part, percent))
                            .collect(),
                    );
                    return Ok((library, members, covered, Some(groups)));
                }
                Ok((library, members, covered, None))
            })
            .await?;
        if let Some(groups) = cached {
            return Ok((library, CropPlan::Complete(groups)));
        }
        let had_coverage = !covered.is_empty();
        let images = self.siftables_for_canonical(group_id, &library).await?;
        Ok((
            library.clone(),
            CropPlan::Incremental {
                library,
                members,
                covered,
                images,
                had_coverage,
            },
        ))
    }

    /// 执行 [`CropPlan`]：全命中直接返回；补算只跑两端任一不在覆盖集里的
    /// 对，新正结果落库、本轮成员写进覆盖标记，最后统一从缓存重建（含
    /// 此前部分扫描攒下的正结果），排序截断与全量路径同一条代码。
    pub async fn crop_scan_run(
        &self,
        group_id: i64,
        plan: CropPlan,
    ) -> Result<Vec<SimilarGroup>, StoreError> {
        let (library, members, covered, images) = match plan {
            CropPlan::Complete(groups) => return Ok(groups),
            CropPlan::Incremental {
                library,
                members,
                covered,
                images,
                ..
            } => (library, members, covered, images),
        };
        let duplicate = crate::config::static_config().duplicate_distance();
        // 两两特征匹配是纯 CPU 的 O(n²)，让出 async worker（同查重）。
        let found = kovi::tokio::task::spawn_blocking(move || {
            let covered: HashSet<&str> = covered.iter().map(String::as_str).collect();
            detect_crops(&images, duplicate, &covered)
        })
        .await
        .map_err(|e| StoreError::Other(anyhow::anyhow!("查裁剪计算线程失败: {e}")))?;
        let fresh: Vec<(String, String, u8)> = found
            .iter()
            .map(|group| match group.hashes.as_slice() {
                [whole, part] => (whole.clone(), part.clone(), group.percent),
                // detect_crops 产出的裁剪组固定两张：整体在前、局部在后。
                _ => unreachable!("裁剪组固定两张"),
            })
            .collect();
        self.with_group(group_id, |pool| async move {
            insert_crop_pairs(&pool, &fresh).await?;
            save_crop_covered(&pool, &library, &members).await?;
            let stored = library_crop_pairs(&pool, &library).await?;
            Ok(assemble_crop_groups(
                stored
                    .into_iter()
                    .map(|(whole, part, percent)| crop_group(&whole, &part, percent))
                    .collect(),
            ))
        })
        .await
    }
}
