use image::{DynamicImage, GrayImage, ImageReader, Limits, imageops::FilterType};
use std::{io::Cursor, sync::LazyLock};

/// 256-bit 感知哈希。dHash 看邻域差分，pHash 看低频 DCT。
/// 比特按行主序切成 4 个 u64 词，`bit / 64` 定词、`bit % 64` 定位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub dhash: [u64; FINGERPRINT_WORDS],
    pub phash: [u64; FINGERPRINT_WORDS],
}

/// 每路哈希的 64-bit 词数，两路共 512 bit。
pub(crate) const FINGERPRINT_WORDS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedImage {
    pub hash: String,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Duplicate,
    Maybe,
    /// hashes 固定两张：第一张是整体，第二张是疑似裁出来的局部。
    Crop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimilarGroup {
    pub kind: GroupKind,
    pub hashes: Vec<String>,
    /// 用分组时那条边的距离换算，越大越像。
    pub percent: u8,
    /// 组员超过 [`MAX_GROUP_MEMBERS`] 被截断或展示超字节预算时置位，标题提示「仅列部分」。
    pub truncated: bool,
}

const DHASH_WIDTH: u32 = 17;
const DHASH_HEIGHT: u32 = 16;
const PHASH_SIZE: u32 = 32;
const PHASH_WINDOW: usize = 16;
const HASH_BITS: u32 = 256;
/// 单个重复组的成员上限。并查集可把整库近似图并成一桶，展示端逐张读 blob，必须设界。
const MAX_GROUP_MEMBERS: usize = 30;
/// 「也许像」组数上限。最坏两两成对是 O(n²)，全量生成会撑爆查重会话内存。
const MAX_MAYBE_GROUPS: usize = 1000;
/// 解码分配上限与宽高边界。入库只限制压缩字节，群成员可用小体积大尺寸图
/// 把解码后的像素缓冲放大成数 GiB；指纹和切图共用这一个受限入口。
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
const MAX_DECODE_WIDTH: u32 = 16384;
/// 长截图高度可达数万像素，高度边界单独放宽。
const MAX_DECODE_HEIGHT: u32 = 65536;

/// 带分配与宽高上限的解码。超限返回 `None`，等价于这张图不适合参与比对或切图。
pub(crate) fn decode_limited(bytes: &[u8]) -> Option<DynamicImage> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_WIDTH);
    limits.max_image_height = Some(MAX_DECODE_HEIGHT);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    reader.decode().ok()
}

/// 几乎没有对比度的图（纯色、近纯色）无法靠感知哈希互相区分。
fn is_flat(gray: &GrayImage) -> bool {
    let mut min = u8::MAX;
    let mut max = 0u8;
    for pixel in gray.pixels() {
        min = min.min(pixel.0[0]);
        max = max.max(pixel.0[0]);
        if max.saturating_sub(min) > 3 {
            return false;
        }
    }
    true
}

pub fn fingerprint_bytes(bytes: &[u8]) -> Option<Fingerprint> {
    let image = decode_limited(bytes)?;
    fingerprint_image(&image)
}

/// 只读文件头拿像素尺寸（JPEG SOF、PNG IHDR 等），不解码。
/// 大图分道用它估算解码后的内存占用——文件字节是被压缩过的，做不了主。
pub(crate) fn pixel_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

fn fingerprint_image(image: &DynamicImage) -> Option<Fingerprint> {
    let gray = image.to_luma8();
    if gray.width() == 0 || gray.height() == 0 || is_flat(&gray) {
        return None;
    }
    Some(Fingerprint {
        dhash: difference_hash(&gray),
        phash: perceptual_hash(&gray),
    })
}

/// 缩到 17×16 后比较左右邻像素，16 行 × 每行 16 次比较 = 256 bit。
/// 对再压缩和轻微缩放稳定。
fn difference_hash(gray: &GrayImage) -> [u64; FINGERPRINT_WORDS] {
    let small = image::imageops::resize(gray, DHASH_WIDTH, DHASH_HEIGHT, FilterType::Triangle);
    let mut bits = [0u64; FINGERPRINT_WORDS];
    for y in 0..DHASH_HEIGHT {
        for x in 0..DHASH_WIDTH - 1 {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            let bit = (y * (DHASH_WIDTH - 1) + x) as usize;
            if left > right {
                bits[bit / 64] |= 1 << (bit % 64);
            }
        }
    }
    bits
}

/// 32×32 DCT 后取 (1,1) 起的 16×16 AC 系数 = 256 bit。
/// 比旧的 8×8 窗口多收中频结构，对低频格局撞车的图区分度更高。
fn perceptual_hash(gray: &GrayImage) -> [u64; FINGERPRINT_WORDS] {
    let small = image::imageops::resize(gray, PHASH_SIZE, PHASH_SIZE, FilterType::Triangle);
    let mut values = [[0.0f64; PHASH_SIZE as usize]; PHASH_SIZE as usize];
    for y in 0..PHASH_SIZE {
        for x in 0..PHASH_SIZE {
            values[y as usize][x as usize] = f64::from(small.get_pixel(x, y).0[0]);
        }
    }
    let dct = dct2_32(&values);

    let mut coeffs = [0.0f64; PHASH_WINDOW * PHASH_WINDOW];
    let mut i = 0;
    // 丢掉 DC，从 (1,1) 取 16×16，避免平均亮度主导比特。
    for row in dct.iter().skip(1).take(PHASH_WINDOW) {
        for coeff in row.iter().skip(1).take(PHASH_WINDOW) {
            coeffs[i] = *coeff;
            i += 1;
        }
    }
    let mut sorted = coeffs;
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = (sorted[127] + sorted[128]) / 2.0;

    let mut bits = [0u64; FINGERPRINT_WORDS];
    for (bit, coeff) in coeffs.iter().enumerate() {
        if *coeff > median {
            bits[bit / 64] |= 1 << (bit % 64);
        }
    }
    bits
}

fn dct2_32(input: &[[f64; 32]; 32]) -> [[f64; 32]; 32] {
    let mut rows = [[0.0f64; 32]; 32];
    let mut tmp = [0.0f64; 32];
    let mut out_1d = [0.0f64; 32];
    for y in 0..32 {
        dct1_32(&input[y], &mut out_1d);
        rows[y] = out_1d;
    }
    let mut cols = [[0.0f64; 32]; 32];
    for x in 0..32 {
        for y in 0..32 {
            tmp[y] = rows[y][x];
        }
        dct1_32(&tmp, &mut out_1d);
        for y in 0..32 {
            cols[y][x] = out_1d[y];
        }
    }
    cols
}

/// DCT 基函数余弦表:`[u][x] = cos(π·(2x+1)·u / 64)`,只依赖 `(u, x)` 下标。
/// 每张图 `dct2_32` 要调 64 次 `dct1_32`,共 65536 次三角函数;查表后仅剩乘加。
/// 表用与原式完全相同的公式预计算,浮点结果逐位一致,指纹与存量数据零漂移。
static DCT_COS: LazyLock<[[f64; 32]; 32]> = LazyLock::new(|| {
    let mut table = [[0.0f64; 32]; 32];
    for (u, row) in table.iter_mut().enumerate() {
        for (x, slot) in row.iter_mut().enumerate() {
            *slot = (std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64 / 64.0).cos();
        }
    }
    table
});

fn dct1_32(input: &[f64; 32], output: &mut [f64; 32]) {
    const N: f64 = 32.0;
    let cos = &*DCT_COS;
    for (u, slot) in output.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (x, value) in input.iter().enumerate() {
            sum += *value * cos[u][x];
        }
        let alpha = if u == 0 {
            (1.0 / N).sqrt()
        } else {
            (2.0 / N).sqrt()
        };
        *slot = alpha * sum;
    }
}

pub fn hamming(a: &[u64; FINGERPRINT_WORDS], b: &[u64; FINGERPRINT_WORDS]) -> u32 {
    // musl 基线目标不含 popcnt，count_ones 会被编译成软件实现（慢数倍）；
    // 部署机 CPU 均支持该指令，运行时检测后走硬件路径。
    #[cfg(target_arch = "x86_64")]
    {
        static HAS_POPCNT: LazyLock<bool> =
            LazyLock::new(|| std::arch::is_x86_feature_detected!("popcnt"));
        if *HAS_POPCNT {
            return unsafe { hamming_popcnt(a, b) };
        }
    }
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
unsafe fn hamming_popcnt(a: &[u64; FINGERPRINT_WORDS], b: &[u64; FINGERPRINT_WORDS]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// 一次算出两路汉明距离的 (max, min)：max 是重复判据，min 是「也许像」判据。
fn pair_distances(a: Fingerprint, b: Fingerprint) -> (u32, u32) {
    let d = hamming(&a.dhash, &b.dhash);
    let p = hamming(&a.phash, &b.phash);
    (d.max(p), d.min(p))
}

pub(crate) fn percent_from_distance(distance: u32) -> u8 {
    let clamped = distance.min(HASH_BITS);
    (((HASH_BITS - clamped) * 100) / HASH_BITS) as u8
}

/// 标题里的「约 x%」反推汉明距离：取仍能显示为至少该百分比的最宽距离。
pub(crate) fn distance_from_percent(percent: u32) -> u32 {
    let percent = percent.min(100);
    (0..=HASH_BITS)
        .rev()
        .find(|&distance| u32::from(percent_from_distance(distance)) >= percent)
        .unwrap_or(0)
}

/// 高置信要求两路都近，连成重复组；其余图之间中等相似才成对标「也许像」。
pub fn cluster(
    images: &[HashedImage],
    duplicate_limit: u32,
    maybe_limit: u32,
) -> Vec<SimilarGroup> {
    let n = images.len();
    if n < 2 {
        return Vec::new();
    }

    let mut parent: Vec<usize> = (0..n).collect();
    let find = |parent: &mut [usize], mut i: usize| {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    };
    let union = |parent: &mut [usize], a: usize, b: usize| {
        let pa = find(parent, a);
        let pb = find(parent, b);
        if pa != pb {
            parent[pa] = pb;
        }
    };

    let mut dup_edges: Vec<(usize, usize, u32)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let (dup_dist, _) = pair_distances(images[i].fingerprint, images[j].fingerprint);
            if dup_dist <= duplicate_limit {
                dup_edges.push((i, j, dup_dist));
                union(&mut parent, i, j);
            }
        }
    }

    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        buckets[find(&mut parent, i)].push(i);
    }

    // 重复边的两端必在同一并查集根下，按根聚合组内最优边，
    // 避免对每个桶全量扫边（表情包库近似图极多时会退化到立方级）。
    let mut best_by_root = vec![HASH_BITS; n];
    for &(a, _b, dist) in &dup_edges {
        let root = find(&mut parent, a);
        best_by_root[root] = best_by_root[root].min(dist);
    }

    let mut in_duplicate = vec![false; n];
    let mut groups = Vec::new();
    for (root, members) in buckets.into_iter().enumerate() {
        if members.len() < 2 {
            continue;
        }
        for &i in &members {
            in_duplicate[i] = true;
        }
        let truncated = members.len() > MAX_GROUP_MEMBERS;
        let mut hashes: Vec<String> = members
            .iter()
            .take(MAX_GROUP_MEMBERS)
            .map(|&i| images[i].hash.clone())
            .collect();
        hashes.sort();
        groups.push(SimilarGroup {
            kind: GroupKind::Duplicate,
            hashes,
            percent: percent_from_distance(best_by_root[root]),
            truncated,
        });
    }

    // 截断时按扫描顺序保留前 MAX_MAYBE_GROUPS 对，不保证剩下的是最相似的；
    // 宁可少提示也不让 O(n²) 的组列表把会话内存撑爆。
    let mut maybe_groups = 0usize;
    'outer: for i in 0..n {
        if in_duplicate[i] {
            continue;
        }
        for j in (i + 1)..n {
            if in_duplicate[j] {
                continue;
            }
            let (dup_dist, maybe_dist) =
                pair_distances(images[i].fingerprint, images[j].fingerprint);
            // 重复判据先过：已是重复组成员的图不参与「也许像」配对。
            if dup_dist <= duplicate_limit || maybe_dist > maybe_limit {
                continue;
            }
            let mut hashes = vec![images[i].hash.clone(), images[j].hash.clone()];
            hashes.sort();
            groups.push(SimilarGroup {
                kind: GroupKind::Maybe,
                hashes,
                percent: percent_from_distance(maybe_dist),
                truncated: false,
            });
            maybe_groups += 1;
            if maybe_groups >= MAX_MAYBE_GROUPS {
                break 'outer;
            }
        }
    }

    groups.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.hashes.cmp(&b.hashes))
    });
    groups
}

// ===== 裁剪检测 =====
// 全局感知哈希对裁剪天然失明：构图一变频谱就漂移，实测中心裁剪保留 96%
// 时 pHash 距离已 88/256。裁剪的判据换成局部特征：B 是 A 的裁剪 ⇔ B 的
// SIFT 特征几乎全部能按一张单应矩阵映射进 A；findHomography 的 RANSAC
// inlier 数就是几何一致的证据，随机图凑不出。链接的 OpenCV 版本由
// pkg-config 决定（指向见 .cargo/config.toml，未入库），SONAME 必须与
// 目标机一致。

use opencv::core::{DMatch, KeyPoint, Mat, NORM_L2, Point2f, Vector};
use opencv::features2d::{BFMatcher, DescriptorMatcherTrait, Feature2DTrait, SIFT};
use opencv::prelude::*;

/// SIFT 归一化画布边长。统一 512×512 再提特征，让单应 RANSAC 的像素
/// 阈值对不同分辨率的图可比；量化后的关键点坐标也落在这个范围。
const SIFT_CANVAS: u32 = 512;
/// 每图最多保留的 SIFT 特征数（按响应排序取前 N）。256 点对单应估计
/// 绰绰有余，同时把每图存储压到 34KB 以内。
const SIFT_MAX_FEATURES: i32 = 512;
/// Lowe 比率测试：最优距离 / 次优距离低于它才算可靠匹配。
const SIFT_LOWE_RATIO: f32 = 0.8;
/// findHomography RANSAC 的重投影阈值（归一化画布上的像素）。
const HOMOGRAPHY_THRESHOLD: f64 = 4.0;
/// 判据双保险：inlier 绝对数（RANSAC 的 4 点解加 2 个独立确认），以及
/// 好匹配的几何内聚率——part 放大后特征密度天然膨胀，inlier 占 part 全部
/// 特征的比例不可用；占好匹配的比例才是「匹配是否几何一致」的度量。
/// 随机对的错误匹配凑不出一致的仿射，内聚率上不去。
const CROP_MIN_INLIERS: u64 = 6;
const CROP_MIN_COHESION_PERCENT: u64 = 40;
/// 全局指纹预筛（通道一）：构图仍相近的轻裁剪，dHash 距离实测 57~124；
/// 随机对 128±11，min 距离超 112 的进不了这通道。
const CROP_PREFILTER: u32 = 112;
/// 中心档预筛（通道二）：重裁剪的构图已经变了，但裁剪图的全图 dHash 应
/// 接近整体图某档中心裁剪的 dHash——直接比内容，随机对撞不上。
/// 档位近似覆盖中心 keep ∈ [33%, 75%]；偏移（非中心）的重裁剪两条通道
/// 都可能漏，是明确的取舍：宁可漏检也不让全库两两跑特征匹配。
const CENTER_KEEPS: [u32; 3] = [75, 50, 33];
const CENTER_PREFILTER: u32 = 96;
/// 裁剪对展示上限，语义同 MAX_MAYBE_GROUPS：防止海量对撑爆查重会话内存。
const MAX_CROP_PAIRS: usize = 500;
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
    let mut sift = SIFT::create(SIFT_MAX_FEATURES, 3, 0.03, 10.0, 1.0).ok()?;
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

/// 全库裁剪两两检测。已是「重复」距离的图对跳过（归查重管）；输出组固定
/// 两张：hashes[0] 是整体、hashes[1] 是局部。
pub fn detect_crops(images: &[SiftableImage], duplicate_limit: u32) -> Vec<SimilarGroup> {
    let n = images.len();
    let mut pairs = Vec::new();
    'outer: for i in 0..n {
        for j in (i + 1)..n {
            let (dup_dist, maybe_dist) =
                pair_distances(images[i].fingerprint, images[j].fingerprint);
            if dup_dist <= duplicate_limit {
                continue;
            }
            // 预筛双通道：轻裁剪构图仍近（全局指纹），重裁剪看中心档内容。
            let i_contains_j = maybe_dist <= CROP_PREFILTER || center_hit(&images[j], &images[i]);
            let j_contains_i = maybe_dist <= CROP_PREFILTER || center_hit(&images[i], &images[j]);
            if !i_contains_j && !j_contains_i {
                continue;
            }
            if i_contains_j && let Some(percent) = match_crop_pair(&images[j].sift, &images[i].sift)
            {
                pairs.push(crop_group(&images[i].hash, &images[j].hash, percent));
            } else if j_contains_i
                && let Some(percent) = match_crop_pair(&images[i].sift, &images[j].sift)
            {
                pairs.push(crop_group(&images[j].hash, &images[i].hash, percent));
            }
            if pairs.len() >= MAX_CROP_PAIRS {
                break 'outer;
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.hashes.cmp(&b.hashes))
    });
    pairs
}

/// part 的全图 dHash 是否命中 whole 的某档中心 dHash。
fn center_hit(part: &SiftableImage, whole: &SiftableImage) -> bool {
    whole
        .sift
        .centers
        .iter()
        .any(|center| hamming(&part.fingerprint.dhash, center) <= CENTER_PREFILTER)
}

fn crop_group(whole: &str, part: &str, percent: u8) -> SimilarGroup {
    SimilarGroup {
        kind: GroupKind::Crop,
        hashes: vec![whole.to_owned(), part.to_owned()],
        percent,
        truncated: false,
    }
}

/// 判定 part 是否 whole 的裁剪局部：Lowe 比率筛出可靠匹配，用 RANSAC
/// 单应的 inlier 数下结论。返回 inlier 占两侧较少一侧特征数的百分比。
fn match_crop_pair(part: &SiftFeatures, whole: &SiftFeatures) -> Option<u8> {
    let n_part = part.points.len();
    let n_whole = whole.points.len();
    if n_part < 4 || n_whole < 4 {
        return None;
    }
    // Mat 的数据指针借用底下的 Vec，两者必须活到匹配结束，不能封成函数返回。
    let part_floats = dequantize(&part.descriptors);
    let whole_floats = dequantize(&whole.descriptors);
    let part_desc =
        Mat::new_rows_cols_with_data(n_part as i32, SIFT_DIMS as i32, &part_floats).ok()?;
    let whole_desc =
        Mat::new_rows_cols_with_data(n_whole as i32, SIFT_DIMS as i32, &whole_floats).ok()?;
    let mut matcher = BFMatcher::new(NORM_L2, false).ok()?;
    matcher.add(&whole_desc).ok()?;
    matcher.train().ok()?;
    let mut matches = Vector::<Vector<DMatch>>::new();
    matcher
        .knn_match(&part_desc, &mut matches, 2, &Mat::default(), false)
        .ok()?;
    let mut src = Vector::<Point2f>::new();
    let mut dst = Vector::<Point2f>::new();
    for i in 0..matches.len() {
        let pair = matches.get(i).ok()?;
        if pair.len() < 2 {
            continue;
        }
        let best = pair.get(0).ok()?;
        let second = pair.get(1).ok()?;
        if best.distance < SIFT_LOWE_RATIO * second.distance {
            let (px, py) = part.points[i];
            let (wx, wy) = whole.points[best.train_idx as usize];
            src.push(Point2f::new(f32::from(px), f32::from(py)));
            dst.push(Point2f::new(f32::from(wx), f32::from(wy)));
        }
    }
    if src.len() < 4 {
        return None;
    }
    let mut mask = Mat::default();
    opencv::calib3d::find_homography(
        &src,
        &dst,
        &mut mask,
        opencv::calib3d::RANSAC,
        HOMOGRAPHY_THRESHOLD,
    )
    .ok()?;
    let mut inliers = 0u64;
    for i in 0..mask.rows() {
        if *mask.at::<u8>(i).ok()? != 0 {
            inliers += 1;
        }
    }
    let good = src.len() as u64;
    if inliers < CROP_MIN_INLIERS || inliers * 100 < good * CROP_MIN_COHESION_PERCENT {
        return None;
    }
    u8::try_from(inliers * 100 / good).ok()
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
    use image::{ImageEncoder, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
    use std::io::Cursor;

    fn patterned(seed: u32) -> RgbImage {
        RgbImage::from_fn(64, 64, |x, y| {
            let v = ((x.wrapping_mul(13) + y.wrapping_mul(7) + seed) % 256) as u8;
            Rgb([v, v.wrapping_add(40), 220u8.wrapping_sub(v)])
        })
    }

    fn png_bytes(image: &RgbImage) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image.clone())
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn jpeg_bytes(image: &RgbImage, quality: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, quality)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        buf
    }

    fn fp(bytes: &[u8]) -> Fingerprint {
        fingerprint_bytes(bytes).expect("fingerprint")
    }

    fn duplicate_distance(a: Fingerprint, b: Fingerprint) -> u32 {
        pair_distances(a, b).0
    }

    /// 把单个 u64 复制成 4 个词，供手工构造指纹的距离关系测试。
    fn words(value: u64) -> [u64; FINGERPRINT_WORDS] {
        [value; FINGERPRINT_WORDS]
    }

    fn hashed(hash: &str, dhash: u64, phash: u64) -> HashedImage {
        HashedImage {
            hash: hash.into(),
            fingerprint: Fingerprint {
                dhash: words(dhash),
                phash: words(phash),
            },
        }
    }

    #[test]
    fn pixel_dimensions_reads_headers() {
        let image = patterned(3);
        assert_eq!(pixel_dimensions(&png_bytes(&image)), Some((64, 64)));
        assert_eq!(pixel_dimensions(&jpeg_bytes(&image, 80)), Some((64, 64)));
        assert_eq!(pixel_dimensions(b"not an image"), None);
    }

    #[test]
    fn dct_cos_table_matches_direct_formula() {
        // 表与逐次 cos() 调用必须逐位一致,否则查表会改变存量指纹。
        for u in 0..32 {
            for x in 0..32 {
                let direct =
                    (std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64 / 64.0).cos();
                assert_eq!(DCT_COS[u][x].to_bits(), direct.to_bits(), "u={u} x={x}");
            }
        }
    }

    #[test]
    fn jpeg_recompress_stays_within_duplicate_limit() {
        let image = patterned(3);
        let high = fp(&jpeg_bytes(&image, 90));
        let low = fp(&jpeg_bytes(&image, 40));
        assert!(
            duplicate_distance(high, low) <= 32,
            "distance {}",
            duplicate_distance(high, low)
        );
    }

    #[test]
    fn unrelated_patterns_are_far() {
        let a = fp(&png_bytes(&patterned(1)));
        let b = fp(&png_bytes(&patterned(200)));
        assert!(
            duplicate_distance(a, b) > 64,
            "distance {}",
            duplicate_distance(a, b)
        );
    }

    #[test]
    fn solid_color_is_skipped() {
        let image = RgbImage::from_pixel(16, 16, Rgb([12, 34, 56]));
        assert!(fingerprint_bytes(&png_bytes(&image)).is_none());
    }

    #[test]
    fn decode_limited_accepts_normal_image() {
        assert!(decode_limited(&png_bytes(&patterned(5))).is_some());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// 只含 IHDR/IEND 的最小 PNG 头，宽高可指定。
    fn png_header_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(&ihdr);
        out.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"IEND");
        out.extend_from_slice(&crc32(b"IEND").to_be_bytes());
        out
    }

    #[test]
    fn decode_limited_rejects_oversized_dimensions() {
        assert!(decode_limited(&png_header_with_dimensions(100_000, 100_000)).is_none());
    }

    #[test]
    fn duplicate_group_members_are_capped() {
        let images: Vec<_> = (0..40)
            .map(|i| hashed(&format!("d{i}"), 0x1111, 0x1111))
            .collect();
        let groups = cluster(&images, 32, 64);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hashes.len(), MAX_GROUP_MEMBERS);
        assert!(groups[0].truncated);
    }

    #[test]
    fn maybe_pairs_are_capped() {
        // 黄金比例常数的相邻倍数两两汉明距离足够远，phash 全同：
        // 整库两两「也许像」而非重复，C(46,2)=1035 超出上限。
        let images: Vec<_> = (1..=46u64)
            .map(|i| {
                hashed(
                    &format!("m{i}"),
                    i.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    0x5555_5555_5555_5555,
                )
            })
            .collect();
        for i in 0..images.len() {
            for j in (i + 1)..images.len() {
                assert!(
                    hamming(&images[i].fingerprint.dhash, &images[j].fingerprint.dhash) > 32,
                    "测试构造不满足两两不重复的前提"
                );
            }
        }
        let groups = cluster(&images, 32, 64);
        assert!(groups.iter().all(|g| g.kind == GroupKind::Maybe));
        assert_eq!(groups.len(), MAX_MAYBE_GROUPS);
    }

    #[test]
    fn cluster_links_high_confidence_and_pairs_leftovers() {
        let images = vec![
            hashed("a", 0x1111, 0x1111),
            hashed("b", 0x1113, 0x1110),
            hashed("c", 0xAAAA_AAAA_AAAA_AAAA, 0x5555_5555_5555_5555),
            hashed("d", 0xAAAA_AAAA_AAAA_AAAB, 0x0),
            hashed("e", 0xFFFF_0000_FFFF_0000, 0x00FF_00FF_00FF_00FF),
            // dHash 接近重复组成员，但不能并进「也许像」。
            hashed("f", 0x1111, u64::MAX),
        ];

        let groups = cluster(&images, 32, 64);
        let dups: Vec<_> = groups
            .iter()
            .filter(|g| g.kind == GroupKind::Duplicate)
            .collect();
        let maybes: Vec<_> = groups
            .iter()
            .filter(|g| g.kind == GroupKind::Maybe)
            .collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].hashes, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(maybes.len(), 1);
        assert_eq!(maybes[0].hashes, vec!["c".to_owned(), "d".to_owned()]);
    }

    #[test]
    fn percent_round_trips_through_title_distance() {
        assert_eq!(percent_from_distance(0), 100);
        assert_eq!(percent_from_distance(32), 87);
        assert_eq!(distance_from_percent(87), 33);
        assert_eq!(
            u32::from(percent_from_distance(distance_from_percent(90))),
            90
        );
    }

    // ===== 裁剪检测 =====

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
        for keep in [50u32, 30] {
            let part = siftable("p", &jpeg_bytes(&center_crop(&base, keep), 85));
            let groups = detect_crops(&[whole.clone(), part], 32);
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
        assert!(detect_crops(&[a, b], 32).is_empty());
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

    /// 手工特征：12 个互异描述子。点铺成 2D 网格——共线点集会让单应
    /// 求解退化，OpenCV 直接把全部点判成 outlier。centers 全零会挡掉
    /// 中心档预筛，配合 cap 测试里全同 dHash 走通道一。
    fn manual_features() -> SiftFeatures {
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

    #[test]
    fn few_shared_features_do_not_form_crop() {
        let mut sparse = manual_features();
        sparse.points.truncate(3);
        sparse.descriptors.truncate(3);
        let whole = manual_features();
        assert_eq!(match_crop_pair(&sparse, &whole), None);
    }

    #[test]
    fn crop_pairs_are_capped() {
        // 同特征两两全命中，C(501,2) 远超上限，验证截断。指纹用黄金比例
        // 拉开 phash 距离避开「重复」跳过，dhash 全同保预筛通过。
        let mut images = Vec::with_capacity(MAX_CROP_PAIRS + 1);
        for i in 0..=MAX_CROP_PAIRS {
            images.push(SiftableImage {
                hash: format!("c{i}"),
                fingerprint: Fingerprint {
                    dhash: words(0),
                    phash: words((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
                },
                sift: manual_features(),
            });
        }
        let groups = detect_crops(&images, 32);
        assert_eq!(groups.len(), MAX_CROP_PAIRS);
        assert!(groups.iter().all(|g| g.kind == GroupKind::Crop));
    }
}
