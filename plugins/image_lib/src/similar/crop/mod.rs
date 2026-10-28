//! 全局感知哈希对裁剪天然失明：构图一变频谱就漂移，实测中心裁剪保留 96%
//! 时 pHash 距离已 88/256。裁剪的判据是局部特征：B 是 A 的裁剪 ⇔ B 的
//! SIFT 特征几乎全部能按一张单应矩阵映射进 A；findHomography 的 RANSAC
//! inlier 数就是几何一致的证据，随机图凑不出。链接的 OpenCV 版本由
//! pkg-config 决定（指向见 .cargo/config.toml，未入库），SONAME 必须与
//! 目标机一致。

mod extract;
// `match` 是关键字，模块名用 pairing，文件仍叫 match.rs。
#[path = "match.rs"]
mod pairing;

#[cfg(test)]
mod tests;

pub use extract::{SiftFeatures, SiftableImage, fingerprint_and_sift};
pub(crate) use extract::{sift_from_bytes, sift_to_bytes};
pub use pairing::detect_crops;
pub(crate) use pairing::{assemble_crop_groups, crop_group};

/// SIFT 归一化画布边长。统一 512×512 再提特征，让单应 RANSAC 的像素
/// 阈值对不同分辨率的图可比；量化后的关键点坐标也落在这个范围。
pub(super) const SIFT_CANVAS: u32 = 512;
/// SIFT 描述子维度（算法固定值，序列化布局依赖它）。
pub(super) const SIFT_DIMS: usize = 128;
/// 中心档数。哈希预筛对「偏移 + 非等比 + 保留 <1/3」的裁剪整对漏检，
/// 同质风格库里也没有更便宜的信号可用，检测走全库两两特征匹配；
/// 常量保留是因为序列化布局依赖它。
pub(super) const CENTER_KEEPS: [u32; 3] = [75, 50, 33];
/// 裁剪配对缓存的算法版本。判据常量或匹配流程一变，缓存的正结果对就
/// 不再可信；bump 此值让 schema_meta 的失效标记换值，整账弃掉重算。
/// v4：覆盖集是每成员一行的行表；knn 用 u8 量化域 VNNI 内核，RANSAC
/// 迭代上限 500。
/// v5：配对正反各查一次再合并——只查单方向时 query 侧恰好是整体的狠
/// 裁剪对会漏检，且结果随成员枚举顺序漂移。量化域利用距离对称，一次
/// 扫描同时得到两个方向的 top-2；点对与两次单向 knn 相同，不另升版本。
pub(crate) const CROP_CACHE_VERSION: &str = "v5";
