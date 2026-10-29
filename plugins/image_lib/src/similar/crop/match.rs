use std::{collections::HashSet, sync::LazyLock};

use opencv::core::{DMatch, Mat, NORM_L2, Point2f, Vector};
use opencv::features2d::{BFMatcher, DescriptorMatcherTraitConst};
use opencv::prelude::*;

use super::extract::{SiftFeatures, SiftableImage};
use super::{SIFT_CANVAS, SIFT_DIMS};
use crate::similar::{GroupKind, SimilarGroup, pair_distances};

/// 匹配时双方只取响应最强的前 N 个特征：真裁剪的好匹配集中在高响应点，
/// 截断把两两比较的计算量压掉四分之三，还顺带砍掉低响应点的弱相似误报。
pub(super) const CROP_MATCH_FEATURES: usize = 384;
/// Lowe 比率测试：最优距离 / 次优距离低于它才算可靠匹配（f32 域回退
/// 路径用）。
const SIFT_LOWE_RATIO: f32 = 0.8;
/// 同一比率在 u8 整数平方距离域的形式：25·d1² < 16·d2² ⟺ d1/d2 < 4/5，
/// 免掉每次比较的浮点转换（u64 承载 25·d1² 的量级）。
pub(super) const LOWE_NUM: u64 = 25;
pub(super) const LOWE_DEN: u64 = 16;
/// findHomography RANSAC 的重投影阈值（归一化画布上的像素）。
const HOMOGRAPHY_THRESHOLD: f64 = 4.0;
/// RANSAC 迭代上限。默认 2000 是为无真模型的负对准备的：置信度永远
/// 收敛不了，每次都烧满预算（负对成本的大头）。恰好压着判据下限的正对
/// （inlier 占比 40%）期望两百次上下就能命中干净样本，500 上限对它
/// 仍绰绰有余。
const CROP_RANSAC_MAX_ITERS: i32 = 500;
const CROP_RANSAC_CONFIDENCE: f64 = 0.995;
/// 判据三保险。小样本陷阱（线上实测）：同系列表情包能凑出 4~10 个弱相似
/// 匹配，RANSAC 用 4 点就能精确解出模型，小 good 必然全 inlier——内聚率
/// 在小样本下毫无辨别力，真实库曾整库刷出 100% 误报。所以好匹配数本身
/// 要有下限，且单应矩阵必须通过把 part 画布四角映射进 whole 的几何审查：
/// 四角落在画布内、覆盖面积比在裁剪合理区间、不翻转。真裁剪的单应天然
/// 满足，垃圾单应四角乱飞。
const CROP_MIN_GOOD: u64 = 12;
const CROP_MIN_INLIERS: u64 = 6;
pub(super) const CROP_MIN_COHESION_PERCENT: u64 = 40;
const CROP_AREA_MIN: f64 = 0.15;
/// 上限压在 1 以下：映射面积比接近 1 的对是同尺寸的同源近重复（实测
/// 线上 100% 误报的主力），那归「查重」管；真裁剪至少裁掉一成内容。
const CROP_AREA_MAX: f64 = 0.9;
/// 四角允许越出画布的宽容（像素），吸收 JPEG 与量化噪声。
const CROP_CORNER_MARGIN: f64 = 32.0;
/// 裁剪对展示上限，语义同 MAX_MAYBE_GROUPS：防止海量对撑爆查重会话内存。
pub(super) const MAX_CROP_PAIRS: usize = 500;

/// 配对热路径的每图静态侧:截断到 [`CROP_MATCH_FEATURES`] 的关键点与
/// 描述子。两两配对下每张图要和其余所有图各配一次,截断与每行常数在
/// 这里一次摊掉,配对循环内只剩匹配本身。缓冲常驻内存(VNNI 形态千张
/// 库约 100MB,f32 回退约 190MB),换热路径零分配。形态由
/// [`quantized_kernel_available`] 在扫描入口统一决定,全库同形态。
pub(super) enum MatchSide {
    /// u8 量化域,自写 knn。query 侧存原值、train 侧存 XOR 0x80 副本:
    /// VNNI 点积 dpbusd 是 u8×i8,一侧原值一侧翻转恰好凑出
    /// Σa·(b−128) = a·b − 128·Σa,修正项只依赖 query 行常数 Σa。
    Quantized(QuantizedSide),
    /// f32 域回退,喂 OpenCV BFMatcher;无 VNNI 的 CPU 走这条路。
    Floats(FloatsSide),
}

/// 64 字节对齐的描述子行。热循环按 32 字节装载，分配只保证 16 字节
/// 对齐时，每行有一条装载会跨缓存行；行长 128 已是 64 的倍数，对齐不
/// 增加内存。
#[repr(align(64))]
#[derive(Clone, Copy)]
pub(super) struct DescRow(pub(super) [u8; SIFT_DIMS]);

impl std::ops::Deref for DescRow {
    type Target = [u8; SIFT_DIMS];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(super) struct QuantizedSide {
    points: Vec<Point2f>,
    /// 描述子原值,配对时作 query 侧。
    pub(super) plain: Vec<DescRow>,
    /// XOR 0x80 副本,i8 解释 = 原值 − 128,配对时作 train 侧。
    pub(super) flipped: Vec<DescRow>,
    /// 每行 Σv,距离重构的修正项。
    pub(super) sum: Vec<i32>,
    /// 每行 ‖v‖²。
    pub(super) norm_sq: Vec<i32>,
}

pub(super) struct FloatsSide {
    points: Vec<Point2f>,
    floats: Vec<f32>,
}

impl MatchSide {
    /// 截断后点数不足 4 的图连 RANSAC 的最小解都凑不出，不参与配对。
    pub(super) fn new(sift: &SiftFeatures, quantized: bool) -> Option<Self> {
        let limit = CROP_MATCH_FEATURES.min(sift.points.len());
        if limit < 4 {
            return None;
        }
        let points = sift.points[..limit]
            .iter()
            .map(|&(x, y)| Point2f::new(f32::from(x), f32::from(y)))
            .collect();
        if quantized {
            let mut plain = Vec::with_capacity(limit);
            let mut flipped = Vec::with_capacity(limit);
            let mut sum = Vec::with_capacity(limit);
            let mut norm_sq = Vec::with_capacity(limit);
            for row in &sift.descriptors[..limit] {
                let mut flipped_row = *row;
                for v in flipped_row.iter_mut() {
                    *v ^= 0x80;
                }
                plain.push(DescRow(*row));
                flipped.push(DescRow(flipped_row));
                sum.push(row.iter().map(|&v| i32::from(v)).sum());
                norm_sq.push(row.iter().map(|&v| i32::from(v) * i32::from(v)).sum());
            }
            Some(Self::Quantized(QuantizedSide {
                points,
                plain,
                flipped,
                sum,
                norm_sq,
            }))
        } else {
            let floats = dequantize(&sift.descriptors[..limit]);
            Some(Self::Floats(FloatsSide { points, floats }))
        }
    }
}

/// VNNI 点积内核:一条指令吃 32/64 维 u8×i8。512 位 AVX512-VNNI 与
/// 256 位 AVX-VNNI 各实现一份;trait 单态化保证内核内联进配对循环。
/// u8 域数据量是 f32 的四分之一,描述子两两比对的带宽压力随之降四倍。
/// 逐对折叠(而非按 train 行分块摊销)是实测结论:块化省下的只是 query
/// 行重复装载,编译器本就把 train 行融进 vpdpbusd 内存操作数并穿插
/// 调度折叠,块化在 AVX-VNNI 上实测反而慢约四分之一。四路独立累加
/// 同样慢约 5%,热循环保持逐对。
trait DotKernel {
    unsafe fn dot(a: &[u8; SIFT_DIMS], b_flipped: &[u8; SIFT_DIMS]) -> i32;
}

struct K512;
struct K256;

impl DotKernel for K512 {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512vnni,avx2")]
    unsafe fn dot(a: &[u8; SIFT_DIMS], b_flipped: &[u8; SIFT_DIMS]) -> i32 {
        use std::arch::x86_64::*;
        // edition 2024 的 unsafe fn 体内不再隐式 unsafe,intrinsic 逐块包好。
        let mut acc = _mm512_setzero_si512();
        for c in 0..SIFT_DIMS / 64 {
            let va = unsafe { _mm512_loadu_si512(a.as_ptr().add(c * 64) as *const __m512i) };
            let vb =
                unsafe { _mm512_loadu_si512(b_flipped.as_ptr().add(c * 64) as *const __m512i) };
            acc = _mm512_dpbusd_epi32(acc, va, vb);
        }
        unsafe { hsum_i32_avx512(acc) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn dot(_: &[u8; SIFT_DIMS], _: &[u8; SIFT_DIMS]) -> i32 {
        unreachable!("非 x86_64 不会构建 Quantized 形态")
    }
}

impl DotKernel for K256 {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,avxvnni")]
    unsafe fn dot(a: &[u8; SIFT_DIMS], b_flipped: &[u8; SIFT_DIMS]) -> i32 {
        use std::arch::x86_64::*;
        let a = a.as_ptr();
        let b = b_flipped.as_ptr();
        let mut acc = _mm256_setzero_si256();
        // 4 次正好覆盖 128 维，展开后 query 行可被提到 train 循环外。
        for off in [0usize, 32, 64, 96] {
            let va = unsafe { _mm256_loadu_si256(a.add(off) as *const __m256i) };
            let vb = unsafe { _mm256_loadu_si256(b.add(off) as *const __m256i) };
            acc = _mm256_dpbusd_avx_epi32(acc, va, vb);
        }
        unsafe { hsum_i32_avx2(acc) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn dot(_: &[u8; SIFT_DIMS], _: &[u8; SIFT_DIMS]) -> i32 {
        unreachable!("非 x86_64 不会构建 Quantized 形态")
    }
}

/// 8 个 i32 通道求和。寄存器内折叠，避免每个 train 行都把累加器
/// 落到栈上再读回来。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum_i32_avx2(v: std::arch::x86_64::__m256i) -> i32 {
    use std::arch::x86_64::*;
    let hi = _mm256_extracti128_si256(v, 1);
    let lo = _mm256_castsi256_si128(v);
    let sum4 = _mm_add_epi32(lo, hi);
    let sum2 = _mm_add_epi32(sum4, _mm_srli_si128(sum4, 8));
    let sum1 = _mm_add_epi32(sum2, _mm_srli_si128(sum2, 4));
    _mm_cvtsi128_si32(sum1)
}

/// 16 个 i32 通道求和。先折成 256 位再走 [`hsum_i32_avx2`]。
/// 不用 `_mm512_reduce_add_epi32`：那条横向折叠在部分虚拟机上是非法指令。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx2")]
unsafe fn hsum_i32_avx512(v: std::arch::x86_64::__m512i) -> i32 {
    use std::arch::x86_64::*;
    let hi = _mm512_extracti64x4_epi64(v, 1);
    let lo = _mm512_castsi512_si256(v);
    unsafe { hsum_i32_avx2(_mm256_add_epi32(lo, hi)) }
}

/// 量化域是否可用:有 512 位或 256 位 VNNI 即走自写 knn。
pub(super) fn quantized_kernel_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        static OK: LazyLock<bool> = LazyLock::new(|| {
            std::arch::is_x86_feature_detected!("avx512vnni")
                || std::arch::is_x86_feature_detected!("avxvnni")
        });
        *OK
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// d² = ‖a‖² + ‖b‖² − 2a·b,其中 a·b = dpb + 128·Σa(dpbusd 输出的是
/// Σa·(b−128))。与 f32 域单调等价,仅距离几乎并列的边界匹配可能翻面
/// (RANSAC 随机性之下的噪声级差异)。128 维 u8 的中间量落在 ±2^24 内,
/// 热循环用 i32,不必每对抬到 i64。
#[inline]
pub(super) fn dist_from_dot(dpb: i32, sum_a: i32, norm_a: i32, norm_b: i32) -> u32 {
    dist_from_base(norm_a - sum_a * 256, norm_b, dpb)
}

/// `base` = ‖a‖² − 256·Σa,对一条 query 行是常数。
#[inline(always)]
fn dist_from_base(base: i32, norm_b: i32, dpb: i32) -> u32 {
    (base + norm_b - dpb * 2) as u32
}

/// 带 target_feature 的核入口。`DotKernel::dot` 带 `#[target_feature]`,
/// Rust 只允许它内联进同样声明该 feature 的函数——直接在普通安全函数
/// 里调泛型 knn 时,每比较一个 train 行都是真正的函数调用,内核小函数
/// 的 call 开销在低频核上占可观比例。外壳把 feature 声明补齐,层级
/// 内联得以发生;安全性由调用点的运行时检测兜底。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vnni,avx2")]
unsafe fn knn_quantized_avx512(
    query: &QuantizedSide,
    train: &QuantizedSide,
    pts: &mut Vec<(Point2f, Point2f)>,
) {
    knn_quantized::<K512>(query, train, pts)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn knn_quantized_avxvnni(
    query: &QuantizedSide,
    train: &QuantizedSide,
    pts: &mut Vec<(Point2f, Point2f)>,
) {
    knn_quantized::<K256>(query, train, pts)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vnni,avx2")]
unsafe fn knn_quantized_both_avx512(
    query: &QuantizedSide,
    train: &QuantizedSide,
    a_to_b: &mut Vec<(Point2f, Point2f)>,
    b_to_a: &mut Vec<(Point2f, Point2f)>,
) {
    knn_quantized_both::<K512>(query, train, a_to_b, b_to_a)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn knn_quantized_both_avxvnni(
    query: &QuantizedSide,
    train: &QuantizedSide,
    a_to_b: &mut Vec<(Point2f, Point2f)>,
    b_to_a: &mut Vec<(Point2f, Point2f)>,
) {
    knn_quantized_both::<K256>(query, train, a_to_b, b_to_a)
}

/// u8 域 knn:每个 query 描述子在 train 侧找精确 top-2,过 Lowe 比率的
/// 点对收进 pts。并列距离取先扫到的行,与 BFMatcher 的严格小于更新
/// 同一选择。整数域比率判据见 [`LOWE_NUM`]。
fn knn_quantized<K: DotKernel>(
    query: &QuantizedSide,
    train: &QuantizedSide,
    pts: &mut Vec<(Point2f, Point2f)>,
) {
    for (qi, qrow) in query.plain.iter().enumerate() {
        let (mut best, mut second) = (u32::MAX, u32::MAX);
        let mut best_t = 0usize;
        for (ti, trow) in train.flipped.iter().enumerate() {
            let d = dist_from_dot(
                unsafe { K::dot(qrow, trow) },
                query.sum[qi],
                query.norm_sq[qi],
                train.norm_sq[ti],
            );
            if d < best {
                second = best;
                best = d;
                best_t = ti;
            } else if d < second {
                second = d;
            }
        }
        if lowe_ok(best, second) {
            pts.push((query.points[qi], train.points[best_t]));
        }
    }
}

/// 距离对称:d(aᵢ, bⱼ) = d(bⱼ, aᵢ)。一次点积同时维护行 top-2(a→b)和
/// 列 top-2(b→a),省掉反向那一整遍 384×384。并列仍取先扫到的下标:
/// 行按 train 下标、列按 query 下标,与两次单向 knn 的扫描顺序一致。
fn knn_quantized_both<K: DotKernel>(
    query: &QuantizedSide,
    train: &QuantizedSide,
    a_to_b: &mut Vec<(Point2f, Point2f)>,
    b_to_a: &mut Vec<(Point2f, Point2f)>,
) {
    let na = query.plain.len();
    let nb = train.flipped.len();
    assert!(na <= CROP_MATCH_FEATURES && nb <= CROP_MATCH_FEATURES);
    // 列状态按 train 下标。只初始化用到的前缀;后续下标访问由上面的
    // 断言保证不越界,热循环走 unchecked。
    let mut col_best = [u32::MAX; CROP_MATCH_FEATURES];
    let mut col_second = [u32::MAX; CROP_MATCH_FEATURES];
    let mut col_at = [0u32; CROP_MATCH_FEATURES];
    for (qi, qrow) in query.plain.iter().enumerate() {
        let base = query.norm_sq[qi] - query.sum[qi] * 256;
        let (mut best, mut second) = (u32::MAX, u32::MAX);
        let mut best_t = 0usize;
        for (ti, trow) in train.flipped.iter().enumerate() {
            let d = dist_from_base(base, train.norm_sq[ti], unsafe { K::dot(qrow, trow) });
            if d < best {
                second = best;
                best = d;
                best_t = ti;
            } else if d < second {
                second = d;
            }
            // SAFETY: ti < nb <= CROP_MATCH_FEATURES,三张表同长。
            unsafe {
                let col_best = col_best.get_unchecked_mut(ti);
                if d < *col_best {
                    *col_second.get_unchecked_mut(ti) = *col_best;
                    *col_best = d;
                    *col_at.get_unchecked_mut(ti) = qi as u32;
                } else {
                    let col_second = col_second.get_unchecked_mut(ti);
                    if d < *col_second {
                        *col_second = d;
                    }
                }
            }
        }
        if lowe_ok(best, second) {
            a_to_b.push((query.points[qi], train.points[best_t]));
        }
    }
    for ti in 0..nb {
        if lowe_ok(col_best[ti], col_second[ti]) {
            b_to_a.push((train.points[ti], query.points[col_at[ti] as usize]));
        }
    }
}

#[inline(always)]
fn lowe_ok(best: u32, second: u32) -> bool {
    second != u32::MAX && LOWE_NUM * u64::from(best) < LOWE_DEN * u64::from(second)
}

/// f32 回退域 knn:BFMatcher 照旧,比率判据维持浮点形式。
fn knn_opencv(
    query: &FloatsSide,
    train: &FloatsSide,
    matcher: &BFMatcher,
    pts: &mut Vec<(Point2f, Point2f)>,
) {
    let query_desc =
        Mat::new_rows_cols_with_data(query.points.len() as i32, SIFT_DIMS as i32, &query.floats)
            .expect("f32 缓冲尺寸自洽");
    let train_desc =
        Mat::new_rows_cols_with_data(train.points.len() as i32, SIFT_DIMS as i32, &train.floats)
            .expect("f32 缓冲尺寸自洽");
    let mut matches = Vector::<Vector<DMatch>>::new();
    if matcher
        .knn_train_match(
            &query_desc,
            &train_desc,
            &mut matches,
            2,
            &Mat::default(),
            false,
        )
        .is_err()
    {
        return;
    }
    for i in 0..matches.len() {
        let Ok(pair) = matches.get(i) else {
            continue;
        };
        if pair.len() < 2 {
            continue;
        }
        let (Ok(best), Ok(second)) = (pair.get(0), pair.get(1)) else {
            continue;
        };
        if best.distance < SIFT_LOWE_RATIO * second.distance {
            pts.push((query.points[i], train.points[best.train_idx as usize]));
        }
    }
}

/// 全库裁剪两两检测。已是「重复」距离的图对跳过（归查重管）；输出组固定
/// 两张：hashes[0] 是整体、hashes[1] 是局部。`covered` 是已比对过的图集合，
/// 两端都在其中的对直接跳过——配对缓存的增量补算靠它只跑有新端点的对。
pub fn detect_crops(
    images: &[SiftableImage],
    duplicate_limit: u32,
    covered: &HashSet<&str>,
) -> Vec<SimilarGroup> {
    detect_crops_inner(
        images,
        duplicate_limit,
        covered,
        quantized_kernel_available(),
    )
}

/// 形态参数供测试强制回退路径:运行时检测只有 VNNI 一条真路,生产
/// 入口 [`detect_crops`] 恒用检测结果。
pub(super) fn detect_crops_inner(
    images: &[SiftableImage],
    duplicate_limit: u32,
    covered: &HashSet<&str>,
    quantized: bool,
) -> Vec<SimilarGroup> {
    let n = images.len();
    if n < 2 {
        return Vec::new();
    }
    // 不做任何预筛：同质风格的表情包库里，哈希/词袋/降维粗匹配全都
    // 分不开「内容重叠」和「风格相似」（实测五种方案全部失效），
    // 预筛只会漏检。配对开销的大头是两侧各 384 个描述子的 knn 暴力
    // 比对，千张库并行数分钟可完成。
    let sides: Vec<Option<MatchSide>> = images
        .iter()
        .map(|image| MatchSide::new(&image.sift, quantized))
        .collect();
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let mut shards: Vec<Vec<SimilarGroup>> = vec![Vec::new(); workers];
    // 交错取行分片，各线程负载均衡；match_crop_pair 是纯函数，可并行。
    // scope 返回前自动 join 全部分片；分片线程 panic 随 scope 上抛，由
    // 调用方的 spawn_blocking 边界转成扫描错误。join 后丢弃结果把 panic
    // 吞掉的话，扫描会照常成功并把丢失的分片记进覆盖集，静默永久缺账。
    std::thread::scope(|scope| {
        for (t, shard) in shards.iter_mut().enumerate() {
            let sides = &sides;
            scope.spawn(move || {
                // BFMatcher 无跨调用状态，分片内复用一个；创建失败视同
                // 该分片全部匹配失败，与单对匹配失败同语义。VNNI 形态
                // 用不上它，创建也免了。
                let matcher = if quantized {
                    None
                } else {
                    let Ok(matcher) = BFMatcher::new(NORM_L2, false) else {
                        return;
                    };
                    Some(matcher)
                };
                let mut i = t;
                while i < n {
                    let Some(side_i) = &sides[i] else {
                        i += workers;
                        continue;
                    };
                    let i_covered = covered.contains(images[i].hash.as_str());
                    for j in (i + 1)..n {
                        if i_covered && covered.contains(images[j].hash.as_str()) {
                            continue;
                        }
                        let Some(side_j) = &sides[j] else {
                            continue;
                        };
                        if let Some(group) = pair_group(
                            &images[i],
                            &images[j],
                            side_i,
                            side_j,
                            matcher.as_ref(),
                            duplicate_limit,
                        ) {
                            shard.push(group);
                        }
                    }
                    i += workers;
                }
            });
        }
    });
    let pairs: Vec<SimilarGroup> = shards.into_iter().flatten().collect();
    assemble_crop_groups(pairs)
}

/// 裁剪组的排序截断：percent 降序、同分按组内哈希字典序，截到展示上限。
/// 全量检测与配对缓存重建两条路径共用，保证产出一致。
pub(crate) fn assemble_crop_groups(mut pairs: Vec<SimilarGroup>) -> Vec<SimilarGroup> {
    pairs.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.hashes.cmp(&b.hashes))
    });
    pairs.truncate(MAX_CROP_PAIRS);
    pairs
}

pub(crate) fn crop_group(whole: &str, part: &str, percent: u8) -> SimilarGroup {
    SimilarGroup {
        kind: GroupKind::Crop,
        hashes: vec![whole.to_owned(), part.to_owned()],
        percent,
        truncated: false,
    }
}

/// 单对判定与产出：已是「重复」距离的对归查重管，不算裁剪；其余交给
/// [`match_crop_pair_both`]，按它的方向把整体放 hashes[0]、局部放
/// hashes[1]。
fn pair_group(
    a: &SiftableImage,
    b: &SiftableImage,
    side_a: &MatchSide,
    side_b: &MatchSide,
    matcher: Option<&BFMatcher>,
    duplicate_limit: u32,
) -> Option<SimilarGroup> {
    let (dup_dist, _) = pair_distances(a.fingerprint, b.fingerprint);
    if dup_dist <= duplicate_limit {
        return None;
    }
    let (a_is_part, percent) = match_crop_pair_both(side_a, side_b, matcher)?;
    let (whole, part) = if a_is_part { (b, a) } else { (a, b) };
    Some(crop_group(&whole.hash, &part.hash, percent))
}

/// 正反两个方向各跑一次 [`match_crop_pair`] 再合并：任一方向成立即检出，
/// 两个都成立取内聚率高的方向。Lowe 方向决定以谁的点提候选对应——
/// query 侧恰好是整体时重叠区外的点全是陪跑，狠裁剪会凑不满
/// [`CROP_MIN_GOOD`]，反向补上这一漏。两个方向都算再合并，结果只依赖
/// 这对图本身，与成员列表的枚举顺序无关。
pub(super) fn match_crop_pair_both(
    a: &MatchSide,
    b: &MatchSide,
    matcher: Option<&BFMatcher>,
) -> Option<(bool, u8)> {
    // 量化域距离对称，一次扫描出两个方向的点对。f32 回退没有自写距离
    // 矩阵，仍各跑一遍 BFMatcher。点对与两次单向 knn 一致，后面的
    // 单应审查顺序也不变。
    let (forward, backward) = if let (MatchSide::Quantized(qa), MatchSide::Quantized(qb)) = (a, b) {
        let mut a_to_b = Vec::new();
        let mut b_to_a = Vec::new();
        #[cfg(target_arch = "x86_64")]
        {
            knn_quantized_both_dispatch(qa, qb, &mut a_to_b, &mut b_to_a);
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (qa, qb);
            unreachable!("非 x86_64 不会构建 Quantized 形态");
        }
        (match_from_points(a_to_b), match_from_points(b_to_a))
    } else {
        (
            match_crop_pair(a, b, matcher),
            match_crop_pair(b, a, matcher),
        )
    };
    merge_directions(forward, backward)
}

fn merge_directions(
    forward: Option<(bool, u8)>,
    backward: Option<(bool, u8)>,
) -> Option<(bool, u8)> {
    match (forward, backward) {
        (Some((a_is_part, pa)), Some((b_is_part, pb))) => {
            // 谁是谁的局部由同一几何给出，两方向的判定必然一致；可能
            // 不同的只有 percent，取内聚率高的方向。
            if pa >= pb {
                Some((a_is_part, pa))
            } else {
                Some((!b_is_part, pb))
            }
        }
        (Some((a_is_part, pa)), None) => Some((a_is_part, pa)),
        (None, Some((b_is_part, pb))) => Some((!b_is_part, pb)),
        (None, None) => None,
    }
}

/// knn 点对收集：query 描述子在 train 侧找 top-2，过 Lowe 比率的点对
/// 收进返回值。内核按 MatchSide 形态分派：量化域走 VNNI 自写（整数
/// 平方距离，判据与 f32 域单调等价），回退域照旧 BFMatcher；`matcher`
/// 仅回退形态需要。全库同形态是 detect_crops 的不变式，混合形态只可能
/// 来自调用方拼错，按匹配失败处理（空点对集，过不了好匹配下限）。
fn collect_point_pairs(
    query: &MatchSide,
    train: &MatchSide,
    matcher: Option<&BFMatcher>,
) -> Vec<(Point2f, Point2f)> {
    let mut pts: Vec<(Point2f, Point2f)> = Vec::new();
    match (query, train) {
        (MatchSide::Quantized(query), MatchSide::Quantized(train)) => {
            #[cfg(target_arch = "x86_64")]
            {
                knn_quantized_dispatch(query, train, &mut pts);
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                unreachable!("非 x86_64 不会构建 Quantized 形态");
            }
        }
        (MatchSide::Floats(query), MatchSide::Floats(train)) => {
            if let Some(matcher) = matcher {
                knn_opencv(query, train, matcher, &mut pts);
            }
        }
        _ => {}
    }
    pts
}

/// 量化域内核分派。检测兜底后进 unsafe 外壳，feature 上下文里内核才能
/// 内联。生产形态由 [`quantized_kernel_available`] 把关，两必居一；测试
/// 可能强设 Quantized，两个都不在时点对留空按无匹配处理，不能让 else
/// 兜底跑 avxvnni——无 VNNI 的 CPU 上那就是 SIGILL。
#[cfg(target_arch = "x86_64")]
pub(super) fn knn_quantized_dispatch(
    query: &QuantizedSide,
    train: &QuantizedSide,
    pts: &mut Vec<(Point2f, Point2f)>,
) {
    static HAS_AVX512: LazyLock<bool> =
        LazyLock::new(|| std::arch::is_x86_feature_detected!("avx512vnni"));
    static HAS_AVXVNNI: LazyLock<bool> =
        LazyLock::new(|| std::arch::is_x86_feature_detected!("avxvnni"));
    if *HAS_AVX512 {
        unsafe { knn_quantized_avx512(query, train, pts) };
    } else if *HAS_AVXVNNI {
        unsafe { knn_quantized_avxvnni(query, train, pts) };
    }
}

/// [`knn_quantized_dispatch`] 的双向版:一次点积填两个方向的点对。
#[cfg(target_arch = "x86_64")]
pub(super) fn knn_quantized_both_dispatch(
    query: &QuantizedSide,
    train: &QuantizedSide,
    a_to_b: &mut Vec<(Point2f, Point2f)>,
    b_to_a: &mut Vec<(Point2f, Point2f)>,
) {
    static HAS_AVX512: LazyLock<bool> =
        LazyLock::new(|| std::arch::is_x86_feature_detected!("avx512vnni"));
    static HAS_AVXVNNI: LazyLock<bool> =
        LazyLock::new(|| std::arch::is_x86_feature_detected!("avxvnni"));
    if *HAS_AVX512 {
        unsafe { knn_quantized_both_avx512(query, train, a_to_b, b_to_a) };
    } else if *HAS_AVXVNNI {
        unsafe { knn_quantized_both_avxvnni(query, train, a_to_b, b_to_a) };
    }
}

/// 单方向的配对判定：跑一次 query→train 的 knn（query 描述子在 train
/// 里找 2-NN）：对应点满足 H 等价于满足 H⁻¹，good/inlier/
/// 内聚率对两个几何审查方向是同一组数字，质量三保险在此只判一次。
/// RANSAC 拟合 H：query→train 后审查双向：H 把 query 画布压进
/// train，则 query 是局部；H⁻¹ 把 train 画布压进 query，则 train 是
/// 局部。返回（query 侧是否局部， inlier 占 good 的百分比）。
/// 方向相关的召回边界见 [`match_crop_pair_both`]，由它正反各调一次
/// 兜住。
pub(super) fn match_crop_pair(
    query: &MatchSide,
    train: &MatchSide,
    matcher: Option<&BFMatcher>,
) -> Option<(bool, u8)> {
    match_from_points(collect_point_pairs(query, train, matcher))
}

/// 已过 Lowe 的点对做单应审查。好匹配数达不到下限的对在这里就出局:
/// 模型质量反正过不了三保险,省下负对上最贵的 RANSAC 迭代预算。
fn match_from_points(pts: Vec<(Point2f, Point2f)>) -> Option<(bool, u8)> {
    let good = pts.len() as u64;
    if good < CROP_MIN_GOOD {
        return None;
    }
    let mut src = Vector::<Point2f>::new();
    let mut dst = Vector::<Point2f>::new();
    for (query_point, train_point) in pts {
        src.push(query_point);
        dst.push(train_point);
    }
    let mut mask = Mat::default();
    let homography = opencv::calib3d::find_homography_ext(
        &src,
        &dst,
        opencv::calib3d::RANSAC,
        HOMOGRAPHY_THRESHOLD,
        &mut mask,
        CROP_RANSAC_MAX_ITERS,
        CROP_RANSAC_CONFIDENCE,
    )
    .ok()?;
    let mut inliers = 0u64;
    for i in 0..mask.rows() {
        if *mask.at::<u8>(i).ok()? != 0 {
            inliers += 1;
        }
    }
    if inliers < CROP_MIN_INLIERS || inliers * 100 < good * CROP_MIN_COHESION_PERCENT {
        return None;
    }
    let percent = u8::try_from(inliers * 100 / good).ok()?;
    let mut matrix = [[0f64; 3]; 3];
    for (r, row) in matrix.iter_mut().enumerate() {
        for (c, slot) in row.iter_mut().enumerate() {
            *slot = *homography.at_2d::<f64>(r as i32, c as i32).ok()?;
        }
    }
    if homography_plausible(&matrix) {
        return Some((true, percent));
    }
    let inverted = invert3(&matrix)?;
    if homography_plausible(&inverted) {
        return Some((false, percent));
    }
    None
}

/// 3×3 矩阵求逆（伴随矩阵法）。det 接近零的退化单应没有可信的逆，
/// 视同几何审查失败。
fn invert3(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let mut inverse = [
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ];
    for row in &mut inverse {
        for slot in row {
            *slot /= det;
        }
    }
    Some(inverse)
}

/// 单应矩阵的几何审查：把源画布四角经 H 投影，要求都落进目标画布
/// （带宽容），四边形面积占目标画布的比例在裁剪合理区间，且没有翻转。
fn homography_plausible(matrix: &[[f64; 3]; 3]) -> bool {
    let side = f64::from(SIFT_CANVAS);
    let corners = [(0.0, 0.0), (side, 0.0), (side, side), (0.0, side)];
    let mut mapped = [(0f64, 0f64); 4];
    for (i, (x, y)) in corners.iter().enumerate() {
        let w = matrix[2][0] * x + matrix[2][1] * y + matrix[2][2];
        if !w.is_finite() || w.abs() < 1e-9 {
            return false;
        }
        mapped[i] = (
            (matrix[0][0] * x + matrix[0][1] * y + matrix[0][2]) / w,
            (matrix[1][0] * x + matrix[1][1] * y + matrix[1][2]) / w,
        );
    }
    if !mapped.iter().all(|(x, y)| {
        *x >= -CROP_CORNER_MARGIN
            && *x <= side + CROP_CORNER_MARGIN
            && *y >= -CROP_CORNER_MARGIN
            && *y <= side + CROP_CORNER_MARGIN
    }) {
        return false;
    }
    // 鞋带公式带符号：负值即翻转，顺带得到面积。
    let area: f64 = mapped
        .iter()
        .zip(mapped.iter().cycle().skip(1))
        .map(|((x1, y1), (x2, y2))| x1 * y2 - x2 * y1)
        .sum::<f64>()
        / 2.0;
    let ratio = area / (side * side);
    (CROP_AREA_MIN..=CROP_AREA_MAX).contains(&ratio)
}

/// 量化描述子还原成浮点（BFMatcher 的 L2 距离要 CV_32F）。
fn dequantize(descriptors: &[[u8; SIFT_DIMS]]) -> Vec<f32> {
    let mut floats = Vec::with_capacity(descriptors.len() * SIFT_DIMS);
    for row in descriptors {
        for &quant in row {
            floats.push(f32::from(quant) / 127.5 - 1.0);
        }
    }
    floats
}
