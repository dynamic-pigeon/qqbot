use std::collections::HashSet;

use image::{Rgb, RgbImage};
use opencv::core::NORM_L2;
use opencv::features2d::BFMatcher;

use super::extract::{
    SiftFeatures, SiftableImage, fingerprint_and_sift, sift_from_bytes, sift_to_bytes,
};
use super::pairing::{
    CROP_MATCH_FEATURES, CROP_MIN_COHESION_PERCENT, LOWE_DEN, LOWE_NUM, MAX_CROP_PAIRS, MatchSide,
    detect_crops, detect_crops_inner, dist_from_dot, match_crop_pair, match_crop_pair_both,
    quantized_kernel_available,
};
#[cfg(target_arch = "x86_64")]
use super::pairing::{knn_quantized_both_dispatch, knn_quantized_dispatch};
use super::{CENTER_KEEPS, SIFT_DIMS};
use crate::similar::tests::{jpeg_bytes, png_bytes, words};
use crate::similar::{FINGERPRINT_WORDS, Fingerprint, GroupKind};

/// 8×8 块状底 + 每 cell 独立斜率的线性斜坡 + 细节抖动：块角是强角点、
/// 各角周围梯度组合互不相同（纯块状图的角点描述子彼此同构，Lowe
/// 比率会把裁剪对的好匹配也滤掉），平滑噪声则一个角点都提不出来。
fn photo_like(seed: u32) -> RgbImage {
    let control = |gx: u32, gy: u32, slot: u32| -> f64 {
        let h = seed
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(gx.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(gy.wrapping_mul(0xC2B2_AE35))
            .wrapping_add(slot.wrapping_mul(0x27D4_EB2F));
        ((h ^ (h >> 16)) % 1000) as f64 / 1000.0
    };
    RgbImage::from_fn(512, 512, |x, y| {
        let (gx, gy) = (x / 16, y / 16);
        let fx = (x % 16) as f64 / 16.0;
        let fy = (y % 16) as f64 / 16.0;
        let base = control(gx, gy, 0) * 200.0;
        // 斜坡幅度要小于块间落差：角点靠块差存活，斜坡只负责让
        // 各角点邻域的梯度组合互不相同。
        let slope = control(gx, gy, 1) * 40.0 - 20.0;
        let detail = ((x.wrapping_mul(31) ^ y.wrapping_mul(17)) % 16) as f64 / 16.0;
        let v = (base + slope * (fx - fy) + detail * 12.0 + 12.0).clamp(0.0, 255.0) as u8;
        Rgb([v, v / 2 + 40, 220u8.saturating_sub(v / 3)])
    })
}

fn center_crop(base: &RgbImage, keep_percent: u32) -> RgbImage {
    let side = 512 * keep_percent / 100;
    let offset = (512 - side) / 2;
    image::imageops::crop_imm(base, offset, offset, side, side).to_image()
}

fn siftable(hash: &str, bytes: &[u8]) -> SiftableImage {
    let (fingerprint, sift) = fingerprint_and_sift(bytes).expect("fingerprint_and_sift");
    SiftableImage {
        hash: hash.into(),
        fingerprint,
        sift,
    }
}

#[test]
fn center_crops_are_detected_with_whole_first() {
    let base = photo_like(3);
    let whole = siftable("w", &jpeg_bytes(&base, 85));
    for keep in [50u32, 40] {
        let part = siftable("p", &jpeg_bytes(&center_crop(&base, keep), 85));
        let groups = detect_crops(&[whole.clone(), part], 32, &HashSet::new());
        assert_eq!(groups.len(), 1, "keep={keep}");
        assert_eq!(groups[0].kind, GroupKind::Crop);
        assert_eq!(groups[0].hashes, vec!["w".to_owned(), "p".to_owned()]);
        assert!(
            groups[0].percent >= CROP_MIN_COHESION_PERCENT as u8,
            "keep={keep} percent={}",
            groups[0].percent
        );
    }
}

#[test]
fn unrelated_images_produce_no_crop_groups() {
    let a = siftable("a", &jpeg_bytes(&photo_like(1), 85));
    let b = siftable("b", &jpeg_bytes(&photo_like(200), 85));
    assert!(detect_crops(&[a, b], 32, &HashSet::new()).is_empty());
}

#[test]
fn covered_pairs_are_skipped_partial_coverage_still_computes() {
    let base = photo_like(3);
    let whole = siftable("w", &jpeg_bytes(&base, 85));
    let part = siftable("p", &jpeg_bytes(&center_crop(&base, 40), 85));
    // 一端在覆盖集：对仍有新端点，照常计算并检出。
    let partial: HashSet<&str> = HashSet::from(["w"]);
    assert_eq!(
        detect_crops(&[whole.clone(), part.clone()], 32, &partial).len(),
        1
    );
    // 两端都覆盖：本轮跳过，不再产出。
    let full: HashSet<&str> = HashSet::from(["w", "p"]);
    assert!(detect_crops(&[whole, part], 32, &full).is_empty());
}

/// 正反两方向的判定由同一几何给出必然一致；percent 可能不同（keep=50
/// 的 f32 域实测正向 75%、反向 74%），合并取高者，且与调用顺序无关。
/// 形态跟随运行时检测，生产上是哪条 knn 内核就测哪条。
#[test]
fn crop_pair_merge_is_order_independent() {
    let quantized = quantized_kernel_available();
    let base = photo_like(3);
    let whole = siftable("w", &jpeg_bytes(&base, 85));
    let part = siftable("p", &jpeg_bytes(&center_crop(&base, 50), 85));
    let side_w = MatchSide::new(&whole.sift, quantized).expect("whole side");
    let side_p = MatchSide::new(&part.sift, quantized).expect("part side");
    let matcher = if quantized {
        None
    } else {
        Some(BFMatcher::new(NORM_L2, false).expect("matcher"))
    };
    let (w_is_part, w_pct) = match_crop_pair(&side_w, &side_p, matcher.as_ref()).expect("forward");
    let (p_is_part, p_pct) = match_crop_pair(&side_p, &side_w, matcher.as_ref()).expect("backward");
    assert_eq!(w_is_part, !p_is_part, "两方向的角色判定必须一致");
    let merged = match_crop_pair_both(&side_w, &side_p, matcher.as_ref()).expect("merged");
    assert_eq!(
        merged,
        (w_is_part, w_pct.max(p_pct)),
        "合并取内聚率高的方向"
    );
    let swapped = match_crop_pair_both(&side_p, &side_w, matcher.as_ref()).expect("merged swapped");
    // swapped 的 bool 指 side_p（它的第一个参数）是否为局部，换回
    // w 的坐标系后再比较。
    assert_eq!((!swapped.0, swapped.1), merged, "合并结果与调用顺序无关");
}

/// 全库检测的结果不随成员枚举顺序变化——单方向版里 query 角色由
/// 顺序决定，percent 会漂移。
#[test]
fn crop_detection_is_independent_of_enumeration_order() {
    let base = photo_like(3);
    let whole = siftable("w", &jpeg_bytes(&base, 85));
    let part = siftable("p", &jpeg_bytes(&center_crop(&base, 40), 85));
    let whole_first = detect_crops(&[whole.clone(), part.clone()], 32, &HashSet::new());
    let part_first = detect_crops(&[part, whole], 32, &HashSet::new());
    assert_eq!(whole_first, part_first);
}

/// 配对内核基准：随机描述子的「无关对」，Lowe 几乎全灭、RANSAC
/// 早退，量到的就是 knn 距离计算本身。手动跑：
/// cargo test --release -p image_lib -- --ignored bench --nocapture
#[test]
#[ignore = "基准测试，手动跑"]
fn bench_crop_pair_kernel() {
    // 测试进程不走插件 main 的全局设置，基准自行压到单线程去抖。
    let _ = opencv::core::set_num_threads(1);
    let quantized = quantized_kernel_available();
    let side = |seed: usize| {
        let mut descriptors = vec![[0u8; SIFT_DIMS]; CROP_MATCH_FEATURES];
        let mut x = 0x9E37_79B9u32 ^ seed as u32;
        for row in descriptors.iter_mut() {
            for v in row.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *v = (x >> 24) as u8;
            }
        }
        let sift = SiftFeatures {
            points: vec![(0, 0); CROP_MATCH_FEATURES],
            descriptors,
            centers: [[0; FINGERPRINT_WORDS]; CENTER_KEEPS.len()],
        };
        MatchSide::new(&sift, quantized).expect("side")
    };
    let matcher = if quantized {
        None
    } else {
        Some(BFMatcher::new(NORM_L2, false).expect("matcher"))
    };
    let sides: Vec<MatchSide> = (0..24).map(side).collect();
    let rounds = 4000usize;
    let start = std::time::Instant::now();
    for k in 0..rounds {
        let a = &sides[k % sides.len()];
        let b = &sides[(k * 7 + 3) % sides.len()];
        std::hint::black_box(match_crop_pair_both(a, b, matcher.as_ref()));
    }
    let elapsed = start.elapsed();
    println!(
        "形态={} match_crop_pair_both: {} 对合计 {:?}, 单对 {:?}",
        if quantized { "VNNI" } else { "BFMatcher" },
        rounds,
        elapsed,
        elapsed / rounds as u32
    );
}

/// 运行时检测只会选中 VNNI 形态，回退路径(BFMatcher)在这里强制
/// 走一遍：无 VNNI 的机器生产上就是这条路。
#[test]
fn crop_detects_on_float_fallback() {
    let base = photo_like(3);
    let whole = siftable("w", &jpeg_bytes(&base, 85));
    let part = siftable("p", &jpeg_bytes(&center_crop(&base, 50), 85));
    let groups = detect_crops_inner(&[whole, part], 32, &HashSet::new(), false);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].kind, GroupKind::Crop);
    assert_eq!(groups[0].hashes, vec!["w".to_owned(), "p".to_owned()]);
    assert!(
        groups[0].percent >= CROP_MIN_COHESION_PERCENT as u8,
        "percent={}",
        groups[0].percent
    );
}

/// 同一对特征走两种 knn 内核，检出结论必须一致：u8 整数距离与
/// f32 距离单调等价，差异只应出现在 RANSAC 随机性本身。用手工
/// 特征对（描述子一一对应，内聚率 100%）避开随机边界。
#[test]
fn quantized_kernel_agrees_with_float_fallback() {
    // 量化内核要求 CPU 有任一形态的 VNNI,没有则跳过——一致性由
    // 有 VNNI 的环境跑本测试覆盖。
    if !quantized_kernel_available() {
        return;
    }
    let whole_sift = manual_whole();
    let part_sift = manual_part();
    let verdicts = [true, false].map(|quantized| {
        let whole = MatchSide::new(&whole_sift, quantized).unwrap();
        let part = MatchSide::new(&part_sift, quantized).unwrap();
        let matcher = if quantized {
            None
        } else {
            Some(BFMatcher::new(NORM_L2, false).unwrap())
        };
        match_crop_pair(&whole, &part, matcher.as_ref())
    });
    // query 是 whole：两个内核都要么检出且判为非局部，要么同不出。
    assert_eq!(verdicts[0].is_some(), verdicts[1].is_some());
    assert_eq!(
        verdicts[0].map(|(is_part, _)| is_part),
        verdicts[1].map(|(is_part, _)| is_part)
    );
}

/// 距离重构的数学本身：dpbusd 输出 Σa·(b−128)，经修正项还原后必须
/// 等于暴力 Σ(a−b)²。SIMD 内核的正确性由双形态一致性测试兜底，
/// 这里钉住标量换算公式。
#[test]
fn dist_from_dot_matches_bruteforce() {
    let row = |seed: u8| -> [u8; SIFT_DIMS] {
        let mut v = seed.wrapping_mul(37);
        let mut row = [0u8; SIFT_DIMS];
        for slot in row.iter_mut() {
            v = v.wrapping_mul(31).wrapping_add(11);
            *slot = v;
        }
        row
    };
    let mut rows = Vec::new();
    for seed in [1u8, 7, 42, 200] {
        rows.push((row(seed), row(seed.wrapping_add(3))));
    }
    rows.push(([0u8; SIFT_DIMS], [0u8; SIFT_DIMS]));
    rows.push(([255; SIFT_DIMS], [0; SIFT_DIMS]));
    rows.push(([255; SIFT_DIMS], [255; SIFT_DIMS]));
    rows.push(([255; SIFT_DIMS], [128; SIFT_DIMS]));
    for (i, (a, b)) in rows.iter().enumerate() {
        let sum_a: i32 = a.iter().map(|&v| i32::from(v)).sum();
        let norm_a: i32 = a.iter().map(|&v| i32::from(v) * i32::from(v)).sum();
        let norm_b: i32 = b.iter().map(|&v| i32::from(v) * i32::from(v)).sum();
        // dpbusd 的点积是 Σa·(b−128)，不是把 b 直接看成 i8。
        let dpb: i32 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| i32::from(x) * (i32::from(y) - 128))
            .sum();
        let brute: u32 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (i32::from(x) - i32::from(y)).unsigned_abs().pow(2))
            .sum::<u32>();
        assert_eq!(dist_from_dot(dpb, sum_a, norm_a, norm_b), brute, "row={i}");
    }
}

/// VNNI 内核与逐对标量 knn 的一致性:随机描述子下两条路产出的点对
/// 下标必须完全一致。train 侧含 query 行的精确副本与重复行,覆盖
/// 距离为零与并列取先扫到行的语义;标量参照独立展开 top-2 更新,
/// 不复用生产代码的路径。
#[test]
#[cfg(target_arch = "x86_64")]
fn simd_knn_matches_scalar_knn() {
    if !quantized_kernel_available() {
        return;
    }
    let mut x = 0x1234_5678u32;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        (x >> 24) as u8
    };
    let mut descriptors_a = vec![[0u8; SIFT_DIMS]; 40];
    for row in &mut descriptors_a {
        for v in row.iter_mut() {
            *v = next();
        }
    }
    let mut descriptors_b = Vec::with_capacity(45);
    descriptors_b.extend_from_slice(&descriptors_a);
    descriptors_b.push(descriptors_a[0]);
    for _ in 0..4 {
        let mut row = [0u8; SIFT_DIMS];
        for v in row.iter_mut() {
            *v = next();
        }
        descriptors_b.push(row);
    }
    // 下标编码进点坐标,u16 直接当行号用。
    let side = |descriptors: Vec<[u8; SIFT_DIMS]>| {
        let sift = SiftFeatures {
            points: (0..descriptors.len() as u16).map(|i| (i, 0)).collect(),
            descriptors,
            centers: [[0; FINGERPRINT_WORDS]; CENTER_KEEPS.len()],
        };
        MatchSide::new(&sift, true).expect("side")
    };
    let (a, b) = (side(descriptors_a), side(descriptors_b));
    let (MatchSide::Quantized(a), MatchSide::Quantized(b)) = (&a, &b) else {
        unreachable!("强设 Quantized 形态")
    };
    let mut pts = Vec::new();
    knn_quantized_dispatch(a, b, &mut pts);
    let mut expected = Vec::new();
    for (qi, qrow) in a.plain.iter().enumerate() {
        let (mut best, mut second) = (u32::MAX, u32::MAX);
        let mut best_t = 0usize;
        for (ti, trow) in b.flipped.iter().enumerate() {
            let dpb: i32 = qrow
                .iter()
                .zip(trow.iter())
                .map(|(&x, &y)| i32::from(x) * i32::from(y as i8))
                .sum();
            let d = dist_from_dot(dpb, a.sum[qi], a.norm_sq[qi], b.norm_sq[ti]);
            if d < best {
                second = best;
                best = d;
                best_t = ti;
            } else if d < second {
                second = d;
            }
        }
        if second != u32::MAX && LOWE_NUM * u64::from(best) < LOWE_DEN * u64::from(second) {
            expected.push(((qi as u16, 0), (best_t as u16, 0)));
        }
    }
    assert!(!expected.is_empty(), "夹具必须产生非平凡点对");
    let key = |pts: &[(opencv::core::Point2f, opencv::core::Point2f)]| -> Vec<_> {
        pts.iter()
            .map(|(q, t)| ((q.x as u16, q.y as u16), (t.x as u16, t.y as u16)))
            .collect::<Vec<_>>()
    };
    let got = key(&pts);
    assert_eq!(got, expected);
    // 一次对称扫描的两个方向必须与两次单向 knn 的点对完全一致,
    // 包括并列距离取先扫到行的下标。
    let mut fwd = Vec::new();
    let mut bwd = Vec::new();
    knn_quantized_both_dispatch(a, b, &mut fwd, &mut bwd);
    assert_eq!(key(&fwd), got);
    let mut rev = Vec::new();
    knn_quantized_dispatch(b, a, &mut rev);
    assert_eq!(key(&bwd), key(&rev));
}

#[test]
fn solid_color_has_no_fingerprint_or_sift() {
    let image = RgbImage::from_pixel(16, 16, Rgb([12, 34, 56]));
    assert!(fingerprint_and_sift(&png_bytes(&image)).is_none());
}

#[test]
fn sift_bytes_round_trip() {
    let features = SiftFeatures {
        points: vec![(1, 2), (60000, 511)],
        descriptors: vec![[7; SIFT_DIMS], [250; SIFT_DIMS]],
        centers: [[0x1111; FINGERPRINT_WORDS]; CENTER_KEEPS.len()],
    };
    let bytes = sift_to_bytes(&features);
    assert_eq!(sift_from_bytes(&bytes), Some(features));
    assert!(sift_from_bytes(&bytes[..bytes.len() - 1]).is_none());
    assert!(sift_from_bytes(&[0; 96]).is_none());
}

/// 手工 whole 特征：12 个互异描述子铺成 2D 网格（共线点集会让单应
/// 求解退化，OpenCV 把全部点判成 outlier）。
fn manual_whole() -> SiftFeatures {
    const SPOTS: [(u16, u16); 12] = [
        (60, 60),
        (300, 60),
        (60, 300),
        (300, 300),
        (180, 180),
        (420, 180),
        (180, 420),
        (420, 420),
        (60, 180),
        (180, 60),
        (300, 420),
        (420, 300),
    ];
    SiftFeatures {
        points: SPOTS.to_vec(),
        descriptors: (0..12u8).map(|i| [i * 17; SIFT_DIMS]).collect(),
        centers: [[0; FINGERPRINT_WORDS]; CENTER_KEEPS.len()],
    }
}

/// 手工 part 特征：whole 的 0.6 倍缩放居中子集，描述子一一对应——
/// 单应映射面积比 0.36，是真裁剪的几何。
fn manual_part() -> SiftFeatures {
    let whole = manual_whole();
    SiftFeatures {
        points: whole
            .points
            .iter()
            .map(|&(x, y)| {
                (
                    (f64::from(x) * 0.6 + 102.0) as u16,
                    (f64::from(y) * 0.6 + 102.0) as u16,
                )
            })
            .collect(),
        descriptors: whole.descriptors.clone(),
        centers: whole.centers,
    }
}

#[test]
fn crop_pairs_are_capped() {
    // whole 簇与 part 簇两两全命中（簇内对是恒等几何、被面积比上限
    // 挡掉），数量远超上限，验证截断。指纹用黄金比例拉开 phash 距离
    // 避开「重复」跳过，dhash 全同保预筛通过。
    let whole = 300;
    let part = 300;
    let mut images = Vec::with_capacity(whole + part);
    for i in 0..whole + part {
        images.push(SiftableImage {
            hash: format!("c{i}"),
            fingerprint: Fingerprint {
                dhash: words(0),
                phash: words((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            },
            sift: if i < whole {
                manual_whole()
            } else {
                manual_part()
            },
        });
    }
    let groups = detect_crops(&images, 32, &HashSet::new());
    assert_eq!(groups.len(), MAX_CROP_PAIRS);
    assert!(groups.iter().all(|g| g.kind == GroupKind::Crop));
}
