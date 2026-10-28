use image::{DynamicImage, imageops::FilterType};
use opencv::core::{KeyPoint, Mat, Vector};
use opencv::features2d::{Feature2DTrait, SIFT};
use opencv::prelude::*;

use super::{CENTER_KEEPS, SIFT_CANVAS, SIFT_DIMS};
use crate::similar::{
    FINGERPRINT_WORDS, Fingerprint, decode_limited, difference_hash, fingerprint_image,
};

/// 提取时保留的 SIFT 特征上限（按响应排序取前 N）。
const SIFT_MAX_FEATURES: i32 = 512;

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
