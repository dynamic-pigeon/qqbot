use image::{DynamicImage, GrayImage, ImageReader, Limits, imageops::FilterType};
use std::{collections::HashSet, io::Cursor, sync::LazyLock};

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
use opencv::features2d::{BFMatcher, DescriptorMatcherTraitConst, Feature2DTrait, SIFT};
use opencv::prelude::*;

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
/// v3：knn 换 u8 量化域 VNNI 内核 + RANSAC 迭代上限 500（检出与 v2
/// 在真实库上逐对一致，仅 RANSAC 随机边界对可能翻面）。
pub(crate) const CROP_CACHE_VERSION: &str = "v3";
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
                        if dup_dist > duplicate_limit {
                            // knn（query=i → train=j）只跑一次，RANSAC
                            // 单应的几何审查双向定谁是谁的局部。
                            if let Some((i_is_part, percent)) =
                                match_crop_pair(side_i, side_j, matcher.as_ref())
                            {
                                let (whole, part) = if i_is_part {
                                    (&images[j].hash, &images[i].hash)
                                } else {
                                    (&images[i].hash, &images[j].hash)
                                };
                                shard.push(crop_group(whole, part, percent));
                            }
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

/// 判定 query 与 train 谁是谁的裁剪局部，只跑一次 knn（query 描述子
/// 在 train 里找 2-NN）：对应点满足 H 等价于满足 H⁻¹，good/inlier/
/// 内聚率对两个几何审查方向是同一组数字，质量三保险在此只判一次。
/// RANSAC 拟合 H：query→train 后审查双向：H 把 query 画布压进
/// train，则 query 是局部；H⁻¹ 把 train 画布压进 query，则 train 是
/// 局部。返回（query 侧是否局部， inlier 占 good 的百分比）。
/// 召回边界：查询侧恰好是整体时，能过 Lowe 的对应只有重叠区里的高
/// 响应点，裁剪越狠越少——凑不满 [`CROP_MIN_GOOD`] 该方向即漏检，
/// 这是单次 knn 相对双向各查一次省一半计算付出的代价。
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
