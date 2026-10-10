//! 全局感知哈希对裁剪天然失明：构图一变频谱就漂移，实测中心裁剪保留 96%
//! 时 pHash 距离已 88/256。裁剪的判据换成局部特征：B 是 A 的裁剪 ⇔ B 的
//! SIFT 特征几乎全部能按一张单应矩阵映射进 A；findHomography 的 RANSAC
//! inlier 数就是几何一致的证据，随机图凑不出。链接的 OpenCV 版本由
//! pkg-config 决定（指向见 .cargo/config.toml，未入库），SONAME 必须与
//! 目标机一致。

use std::{collections::HashSet, sync::LazyLock};

use image::{DynamicImage, imageops::FilterType};
use opencv::core::{DMatch, KeyPoint, Mat, NORM_L2, Point2f, Vector};
use opencv::features2d::{BFMatcher, DescriptorMatcherTraitConst, Feature2DTrait, SIFT};
use opencv::prelude::*;

use super::{
    FINGERPRINT_WORDS, Fingerprint, GroupKind, SimilarGroup, decode_limited, difference_hash,
    fingerprint_image, pair_distances,
};

/// SIFT 归一化画布边长。统一 512×512 再提特征，让单应 RANSAC 的像素
/// 阈值对不同分辨率的图可比；量化后的关键点坐标也落在这个范围。
const SIFT_CANVAS: u32 = 512;
/// 提取时保留的 SIFT 特征上限（按响应排序取前 N）。
const SIFT_MAX_FEATURES: i32 = 512;
/// 匹配时双方只取响应最强的前 N 个特征：真裁剪的好匹配集中在高响应点，
/// 截断把两两比较的计算量压掉四分之三，还顺带砍掉低响应点的弱相似误报。
const CROP_MATCH_FEATURES: usize = 384;
/// Lowe 比率测试：最优距离 / 次优距离低于它才算可靠匹配（f32 域回退
/// 路径用）。
const SIFT_LOWE_RATIO: f32 = 0.8;
/// 同一比率在 u8 整数平方距离域的形式：25·d1² < 16·d2² ⟺ d1/d2 < 4/5，
/// 免掉每次比较的浮点转换（u64 承载 25·d1² 的量级）。
const LOWE_NUM: u64 = 25;
const LOWE_DEN: u64 = 16;
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
const CROP_MIN_COHESION_PERCENT: u64 = 40;
const CROP_AREA_MIN: f64 = 0.15;
/// 上限压在 1 以下：映射面积比接近 1 的对是同尺寸的同源近重复（实测
/// 线上 100% 误报的主力），那归「查重」管；真裁剪至少裁掉一成内容。
const CROP_AREA_MAX: f64 = 0.9;
/// 四角允许越出画布的宽容（像素），吸收 JPEG 与量化噪声。
const CROP_CORNER_MARGIN: f64 = 32.0;
/// 中心档数。历史上有过用它做哈希预筛的版本——实测对「偏移 + 非等比 +
/// 保留 <1/3」的裁剪整对漏检（同质风格库里也没有任何更便宜的信号可用），
/// 预筛已删，检测走全库两两特征匹配；常量保留是因为序列化布局依赖它。
const CENTER_KEEPS: [u32; 3] = [75, 50, 33];
/// 裁剪对展示上限，语义同 MAX_MAYBE_GROUPS：防止海量对撑爆查重会话内存。
const MAX_CROP_PAIRS: usize = 500;
/// 裁剪配对缓存的算法版本。判据常量或匹配流程一变，缓存的正结果对就
/// 不再可信；bump 此值让 schema_meta 的失效标记换值，整账弃掉重算。
/// v4：覆盖集从每库一行的拼接字符串改为每成员一行的行表；knn 换 u8
/// 量化域 VNNI 内核，RANSAC 迭代上限 500。VNNI 检出与旧判据在真实库
/// 上逐对一致，仅 RANSAC 随机边界对可能翻面。
/// v5：配对从单方向改为正反各查一次再合并——query 侧恰好是整体的狠
/// 裁剪对会漏检，且结果随成员枚举顺序漂移，正结果集变化。
pub(crate) const CROP_CACHE_VERSION: &str = "v5";
/// SIFT 描述子维度（算法固定值，序列化布局依赖它）。
const SIFT_DIMS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiftFeatures {
    /// 归一化画布上的关键点坐标，与 descriptors 按下标对齐。
    pub points: Vec<(u16, u16)>,
    /// 量化到 u8 的 128 维描述子。
    pub descriptors: Vec<[u8; SIFT_DIMS]>,
    /// 三档中心裁剪的 256-bit dHash，按下标对应 [`CENTER_KEEPS`]。
    pub centers: [[u64; FINGERPRINT_WORDS]; CENTER_KEEPS.len()],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiftableImage {
    pub hash: String,
    pub fingerprint: Fingerprint,
    pub sift: SiftFeatures,
}

/// 一次解码同时产出全局指纹与 SIFT 特征：懒回填管道里两张表共用这次解码。
pub fn fingerprint_and_sift(bytes: &[u8]) -> Option<(Fingerprint, SiftFeatures)> {
    let image = decode_limited(bytes)?;
    Some((fingerprint_image(&image)?, sift_features(&image)?))
}

/// OpenCV 4.7 起 `SIFT::create` 追加 `enable_precise_upscale` 尾参，按
/// build.rs 下发的 `opencv_ge_4_7` cfg 分新旧签名。≥4.7 传 false（官方
/// 默认；true 在 4.10 上实测会让部分裁剪对掉到内聚率阈值之下）。特征与
/// 4.6 产物不逐位一致，跨版本混算靠 sift 表算法版本整体弃账兜底。
fn create_sift() -> opencv::Result<opencv::core::Ptr<SIFT>> {
    #[cfg(opencv_ge_4_7)]
    let sift = SIFT::create(SIFT_MAX_FEATURES, 3, 0.03, 10.0, 1.0, false);
    #[cfg(not(opencv_ge_4_7))]
    let sift = SIFT::create(SIFT_MAX_FEATURES, 3, 0.03, 10.0, 1.0);
    sift
}

/// 提取 SIFT 特征。解码限额由 [`decode_limited`] 把关，这里只把灰度像素
/// 装进 CV_8U Mat，不走 imdecode。
fn sift_features(image: &DynamicImage) -> Option<SiftFeatures> {
    // Lanczos3 保边缘锐度：放大图上采样后的特征描述子要和原图的对应点
    // 匹配，软核（Triangle）会把边缘抹软、好匹配大幅减少。
    let gray = image::imageops::resize(
        &image.to_luma8(),
        SIFT_CANVAS,
        SIFT_CANVAS,
        FilterType::Lanczos3,
    );
    let mat =
        Mat::new_rows_cols_with_data(SIFT_CANVAS as i32, SIFT_CANVAS as i32, gray.as_raw()).ok()?;
    let mut sift = create_sift().ok()?;
    let mut keypoints = Vector::<KeyPoint>::new();
    let mut descriptors = Mat::default();
    sift.detect_and_compute(
        &mat,
        &Mat::default(),
        &mut keypoints,
        &mut descriptors,
        false,
    )
    .ok()?;
    let mut points = Vec::with_capacity(keypoints.len());
    for keypoint in keypoints.iter() {
        points.push((keypoint.pt().x as u16, keypoint.pt().y as u16));
    }
    let rows = descriptors.rows();
    if rows < 1 || points.len() != rows as usize {
        return None;
    }
    let mut quants = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut row = [0u8; SIFT_DIMS];
        for (c, slot) in row.iter_mut().enumerate() {
            let value = *descriptors.at_2d::<f32>(r, c as i32).ok()?;
            // SIFT 描述子 L2 归一化后元素幅值在 [-1, 1]，线性量化到 u8。
            *slot = ((value + 1.0) * 127.5).clamp(0.0, 255.0) as u8;
        }
        quants.push(row);
    }
    Some(SiftFeatures {
        points,
        descriptors: quants,
        centers: center_hashes(image),
    })
}

/// 三档中心裁剪各自的 256-bit dHash。裁剪检测的预筛拿它和候选局部的
/// 全图 dHash 直接比内容。
fn center_hashes(image: &DynamicImage) -> [[u64; FINGERPRINT_WORDS]; CENTER_KEEPS.len()] {
    let gray = image.to_luma8();
    let (width, height) = (gray.width(), gray.height());
    let mut out = [[0u64; FINGERPRINT_WORDS]; CENTER_KEEPS.len()];
    for (slot, &keep) in out.iter_mut().zip(&CENTER_KEEPS) {
        let crop_w = (width * keep / 100).max(1).min(width);
        let crop_h = (height * keep / 100).max(1).min(height);
        let crop = image::imageops::crop_imm(
            &gray,
            (width - crop_w) / 2,
            (height - crop_h) / 2,
            crop_w,
            crop_h,
        )
        .to_image();
        *slot = difference_hash(&crop);
    }
    out
}

/// 配对热路径的每图静态侧:截断到 [`CROP_MATCH_FEATURES`] 的关键点与
/// 描述子。两两配对下每张图要和其余所有图各配一次,截断与每行常数在
/// 这里一次摊掉,配对循环内只剩匹配本身。缓冲常驻内存(VNNI 形态千张
/// 库约 100MB,f32 回退约 190MB),换热路径零分配。形态由
/// [`quantized_kernel_available`] 在扫描入口统一决定,全库同形态。
enum MatchSide {
    /// u8 量化域,自写 knn。query 侧存原值、train 侧存 XOR 0x80 副本:
    /// VNNI 点积 dpbusd 是 u8×i8,一侧原值一侧翻转恰好凑出
    /// Σa·(b−128) = a·b − 128·Σa,修正项只依赖 query 行常数 Σa。
    Quantized(QuantizedSide),
    /// f32 域回退,喂 OpenCV BFMatcher;无 VNNI 的 CPU 走这条路,
    /// 与旧版行为一致。
    Floats(FloatsSide),
}

struct QuantizedSide {
    points: Vec<Point2f>,
    /// 描述子原值,配对时作 query 侧。
    plain: Vec<[u8; SIFT_DIMS]>,
    /// XOR 0x80 副本,i8 解释 = 原值 − 128,配对时作 train 侧。
    flipped: Vec<[u8; SIFT_DIMS]>,
    /// 每行 Σv,距离重构的修正项。
    sum: Vec<i32>,
    /// 每行 ‖v‖²。
    norm_sq: Vec<i32>,
}

struct FloatsSide {
    points: Vec<Point2f>,
    floats: Vec<f32>,
}

impl MatchSide {
    /// 截断后点数不足 4 的图连 RANSAC 的最小解都凑不出，不参与配对。
    fn new(sift: &SiftFeatures, quantized: bool) -> Option<Self> {
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
                plain.push(*row);
                flipped.push(flipped_row);
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

/// VNNI 点积内核:一条指令吃 32/64 维 u8×i8。部署机( Cascade Lake)
/// 只有 512 位 AVX512-VNNI,桌面平台是 256 位 AVX-VNNI,各实现一份;
/// trait 单态化保证内核内联进配对循环。u8 域数据量是 f32 的四分之一,
/// 描述子两两比对的带宽压力随之降四倍——部署机实测整体 2.7×。
trait DotKernel {
    unsafe fn dot(a: &[u8; SIFT_DIMS], b_flipped: &[u8; SIFT_DIMS]) -> i32;
}

struct K512;
struct K256;

impl DotKernel for K512 {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512vnni")]
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
        // 不用 _mm512_reduce_add_epi32:横向折叠序列在部分虚拟化环境
        // 踩非法指令,store 回内存再标量求和只依赖 F+VNNI 基础指令。
        let mut tmp = [0i32; 16];
        unsafe { _mm512_storeu_si512(tmp.as_mut_ptr() as *mut __m512i, acc) };
        tmp.iter().sum()
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
        let mut acc = _mm256_setzero_si256();
        for c in 0..SIFT_DIMS / 32 {
            let va = unsafe { _mm256_loadu_si256(a.as_ptr().add(c * 32) as *const __m256i) };
            let vb =
                unsafe { _mm256_loadu_si256(b_flipped.as_ptr().add(c * 32) as *const __m256i) };
            acc = _mm256_dpbusd_avx_epi32(acc, va, vb);
        }
        let mut tmp = [0i32; 8];
        unsafe { _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, acc) };
        tmp.iter().sum()
    }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn dot(_: &[u8; SIFT_DIMS], _: &[u8; SIFT_DIMS]) -> i32 {
        unreachable!("非 x86_64 不会构建 Quantized 形态")
    }
}

/// 量化域是否可用:有 512 位或 256 位 VNNI 即走自写 knn。云主机 CPU
/// flags 有虚报前科,换部署目标时先实测(2026-10-08 当前部署机验证可跑)。
fn quantized_kernel_available() -> bool {
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
/// Σa·(b−128))。全程 i32/i64 整数域:与 f32 域单调等价,仅距离几乎
/// 并列的边界匹配可能翻面(RANSAC 随机性之下的噪声级差异)。
#[inline]
fn dist_from_dot(dpb: i32, sum_a: i32, norm_a: i32, norm_b: i32) -> u32 {
    let ab = i64::from(dpb) + 128 * i64::from(sum_a);
    (i64::from(norm_a) + i64::from(norm_b) - 2 * ab) as u32
}

/// 带 target_feature 的核入口。`DotKernel::dot` 带 `#[target_feature]`,
/// Rust 只允许它内联进同样声明该 feature 的函数——直接在普通安全函数
/// 里调泛型 knn 时,每比较一个 train 行都是真正的函数调用,内核小函数
/// 的 call 开销在低频核上占可观比例。外壳把 feature 声明补齐,层级
/// 内联得以发生;安全性由调用点的运行时检测兜底。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vnni")]
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
        if second != u32::MAX && LOWE_NUM * u64::from(best) < LOWE_DEN * u64::from(second) {
            pts.push((query.points[qi], train.points[best_t]));
        }
    }
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

/// 把 OpenCV 全局线程池压到单线程的守卫。配对分片线程已经打满核，
/// 匹配器内部的 parallel_for 再起线程池只会嵌套超订阅互相踩；提取等
/// 非配对路径仍要原来的并行度，离开作用域时恢复原值。
struct SingleThreadGuard(Option<i32>);

impl SingleThreadGuard {
    fn new() -> Self {
        let saved = opencv::core::get_num_threads().ok();
        let _ = opencv::core::set_num_threads(1);
        Self(saved)
    }
}

impl Drop for SingleThreadGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.0 {
            let _ = opencv::core::set_num_threads(saved);
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
fn detect_crops_inner(
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
    // 交错取行分片，各线程负载均衡；match_crop_pair 是纯函数，可并行。
    let sides: Vec<Option<MatchSide>> = images
        .iter()
        .map(|image| MatchSide::new(&image.sift, quantized))
        .collect();
    let _single_thread = SingleThreadGuard::new();
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let mut shards: Vec<Vec<SimilarGroup>> = vec![Vec::new(); workers];
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for (t, shard) in shards.iter_mut().enumerate() {
            let sides = &sides;
            handles.push(scope.spawn(move || {
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
                    for j in (i + 1)..n {
                        if covered.contains(images[i].hash.as_str())
                            && covered.contains(images[j].hash.as_str())
                        {
                            continue;
                        }
                        let Some(side_j) = &sides[j] else {
                            continue;
                        };
                        let (dup_dist, _) =
                            pair_distances(images[i].fingerprint, images[j].fingerprint);
                        // 「重复」距离的对归查重管，不算裁剪。
                        if dup_dist > duplicate_limit
                            && let Some((i_is_part, percent)) =
                                match_crop_pair_both(side_i, side_j, matcher.as_ref())
                        {
                            let (whole, part) = if i_is_part {
                                (&images[j].hash, &images[i].hash)
                            } else {
                                (&images[i].hash, &images[j].hash)
                            };
                            shard.push(crop_group(whole, part, percent));
                        }
                    }
                    i += workers;
                }
            }));
        }
        for handle in handles {
            let _ = handle.join();
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

/// 正反两个方向各跑一次 [`match_crop_pair`] 再合并：任一方向成立即检出，
/// 两个都成立取内聚率高的方向。Lowe 方向决定以谁的点提候选对应——
/// query 侧恰好是整体时重叠区外的点全是陪跑，狠裁剪会凑不满
/// [`CROP_MIN_GOOD`]，反向补上这一漏。两个方向都算再合并，结果只依赖
/// 这对图本身，与成员列表的枚举顺序无关。
fn match_crop_pair_both(
    a: &MatchSide,
    b: &MatchSide,
    matcher: Option<&BFMatcher>,
) -> Option<(bool, u8)> {
    let forward = match_crop_pair(a, b, matcher);
    let backward = match_crop_pair(b, a, matcher);
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

/// 单方向的配对判定：跑一次 query→train 的 knn（query 描述子在 train
/// 里找 2-NN）：对应点满足 H 等价于满足 H⁻¹，good/inlier/
/// 内聚率对两个几何审查方向是同一组数字，质量三保险在此只判一次。
/// RANSAC 拟合 H：query→train 后审查双向：H 把 query 画布压进
/// train，则 query 是局部；H⁻¹ 把 train 画布压进 query，则 train 是
/// 局部。返回（query 侧是否局部， inlier 占 good 的百分比）。
/// 方向相关的召回边界见 [`match_crop_pair_both`]，由它正反各调一次
/// 兜住。
/// knn 内核按 MatchSide 形态分派:量化域走 VNNI 自写(整数平方距离,
/// 判据与 f32 域单调等价),回退域照旧 BFMatcher;`matcher` 仅回退
/// 形态需要。两条路产出的点对集进同一段 RANSAC。
fn match_crop_pair(
    query: &MatchSide,
    train: &MatchSide,
    matcher: Option<&BFMatcher>,
) -> Option<(bool, u8)> {
    let mut pts: Vec<(Point2f, Point2f)> = Vec::new();
    match (query, train) {
        // 全库同形态是 detect_crops 的不变式,混合形态只可能来自调用
        // 方拼错,按匹配失败处理。
        (MatchSide::Quantized(query), MatchSide::Quantized(train)) => {
            #[cfg(target_arch = "x86_64")]
            {
                static K512: LazyLock<bool> =
                    LazyLock::new(|| std::arch::is_x86_feature_detected!("avx512vnni"));
                // 检测兜底后进 unsafe 外壳,feature 上下文里内核才能内联。
                if *K512 {
                    unsafe { knn_quantized_avx512(query, train, &mut pts) };
                } else {
                    unsafe { knn_quantized_avxvnni(query, train, &mut pts) };
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            unreachable!("非 x86_64 不会构建 Quantized 形态");
        }
        (MatchSide::Floats(query), MatchSide::Floats(train)) => {
            let matcher = matcher?;
            knn_opencv(query, train, matcher, &mut pts);
        }
        _ => return None,
    }
    // 好匹配数达不到下限的对在这里就出局：模型质量反正过不了三保险，
    // 省下负对上最贵的 RANSAC 迭代预算。
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

/// 序列化：三档中心哈希 + u16 点数 + 每点 (x, y, 128 字节描述子)，全大端。
pub(crate) fn sift_to_bytes(features: &SiftFeatures) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + features.points.len() * (4 + SIFT_DIMS));
    for center in &features.centers {
        for word in center {
            out.extend_from_slice(&word.to_be_bytes());
        }
    }
    out.extend_from_slice(&(features.points.len() as u16).to_be_bytes());
    for ((x, y), descriptor) in features.points.iter().zip(&features.descriptors) {
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(descriptor);
    }
    out
}

pub(crate) fn sift_from_bytes(bytes: &[u8]) -> Option<SiftFeatures> {
    let center_bytes = CENTER_KEEPS.len() * FINGERPRINT_WORDS * 8;
    let mut centers = [[0u64; FINGERPRINT_WORDS]; CENTER_KEEPS.len()];
    for (grid, slot) in centers.iter_mut().enumerate() {
        for (word, i) in slot.iter_mut().zip(0..FINGERPRINT_WORDS) {
            let start = grid * FINGERPRINT_WORDS * 8 + i * 8;
            *word = u64::from_be_bytes(bytes.get(start..start + 8)?.try_into().ok()?);
        }
    }
    let body = &bytes[center_bytes..];
    let count = u16::from_be_bytes(body.get(..2)?.try_into().ok()?) as usize;
    let stride = 4 + SIFT_DIMS;
    if body.len() != 2 + count * stride {
        return None;
    }
    let mut points = Vec::with_capacity(count);
    let mut descriptors = Vec::with_capacity(count);
    for i in 0..count {
        let record = &body[2 + i * stride..2 + (i + 1) * stride];
        let x = u16::from_be_bytes(record[..2].try_into().ok()?);
        let y = u16::from_be_bytes(record[2..4].try_into().ok()?);
        points.push((x, y));
        descriptors.push(record[4..].try_into().ok()?);
    }
    Some(SiftFeatures {
        points,
        descriptors,
        centers,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::similar::tests::{jpeg_bytes, png_bytes, words};
    use image::{Rgb, RgbImage};

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
        let (w_is_part, w_pct) =
            match_crop_pair(&side_w, &side_p, matcher.as_ref()).expect("forward");
        let (p_is_part, p_pct) =
            match_crop_pair(&side_p, &side_w, matcher.as_ref()).expect("backward");
        assert_eq!(w_is_part, !p_is_part, "两方向的角色判定必须一致");
        let merged = match_crop_pair_both(&side_w, &side_p, matcher.as_ref()).expect("merged");
        assert_eq!(
            merged,
            (w_is_part, w_pct.max(p_pct)),
            "合并取内聚率高的方向"
        );
        let swapped =
            match_crop_pair_both(&side_p, &side_w, matcher.as_ref()).expect("merged swapped");
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
        let _guard = SingleThreadGuard::new();
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
    /// 走一遍：无 VNNI 的机器（CI runner 等）生产上就是这条路。
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
        for seed in [1u8, 7, 42, 200] {
            let a = row(seed);
            let b = row(seed.wrapping_add(3));
            let sum_a: i32 = a.iter().map(|&v| i32::from(v)).sum();
            let norm_a: i32 = a.iter().map(|&v| i32::from(v) * i32::from(v)).sum();
            let norm_b: i32 = b.iter().map(|&v| i32::from(v) * i32::from(v)).sum();
            let dpb: i32 = a
                .iter()
                .zip(&b)
                .map(|(&x, &y)| i32::from(x) * i32::from(y as i8))
                .sum();
            let brute: u32 = a
                .iter()
                .zip(&b)
                .map(|(&x, &y)| (i32::from(x) - i32::from(y)).unsigned_abs().pow(2))
                .sum::<u32>();
            assert_eq!(
                dist_from_dot(dpb, sum_a, norm_a, norm_b),
                brute,
                "seed={seed}"
            );
        }
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
}
