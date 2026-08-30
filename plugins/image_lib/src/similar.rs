use image::{DynamicImage, GrayImage, imageops::FilterType};

/// 64-bit 感知哈希。dHash 看邻域差分，pHash 看低频 DCT。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub dhash: u64,
    pub phash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedImage {
    pub hash: String,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Duplicate,
    Maybe,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimilarGroup {
    pub kind: GroupKind,
    pub hashes: Vec<String>,
    /// 用分组时那条边的距离换算，越大越像。
    pub percent: u8,
}

const DHASH_WIDTH: u32 = 9;
const DHASH_HEIGHT: u32 = 8;
const PHASH_SIZE: u32 = 32;
const PHASH_WINDOW: usize = 8;
const HASH_BITS: u32 = 64;

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
    let image = image::load_from_memory(bytes).ok()?;
    fingerprint_image(&image)
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

/// 缩到 9×8 后比较左右邻像素。对再压缩和轻微缩放稳定。
fn difference_hash(gray: &GrayImage) -> u64 {
    let small = image::imageops::resize(gray, DHASH_WIDTH, DHASH_HEIGHT, FilterType::Triangle);
    let mut bits = 0u64;
    let mut bit = 0u32;
    for y in 0..DHASH_HEIGHT {
        for x in 0..DHASH_WIDTH - 1 {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            if left > right {
                bits |= 1 << bit;
            }
            bit += 1;
        }
    }
    bits
}

/// 32×32 DCT 后取最低频 8×8 AC 系数。对滤镜、调色比纯差分稳。
fn perceptual_hash(gray: &GrayImage) -> u64 {
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
    // 丢掉 DC，从 (1,1) 取 8×8，避免平均亮度主导比特。
    for row in dct.iter().skip(1).take(PHASH_WINDOW) {
        for coeff in row.iter().skip(1).take(PHASH_WINDOW) {
            coeffs[i] = *coeff;
            i += 1;
        }
    }
    let mut sorted = coeffs;
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = (sorted[31] + sorted[32]) / 2.0;

    let mut bits = 0u64;
    for (bit, coeff) in coeffs.iter().enumerate() {
        if *coeff > median {
            bits |= 1 << bit;
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

fn dct1_32(input: &[f64; 32], output: &mut [f64; 32]) {
    const N: f64 = 32.0;
    for (u, slot) in output.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (x, value) in input.iter().enumerate() {
            sum += *value
                * (std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64 / (2.0 * N)).cos();
        }
        let alpha = if u == 0 {
            (1.0 / N).sqrt()
        } else {
            (2.0 / N).sqrt()
        };
        *slot = alpha * sum;
    }
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn duplicate_distance(a: Fingerprint, b: Fingerprint) -> u32 {
    hamming(a.dhash, b.dhash).max(hamming(a.phash, b.phash))
}

fn maybe_distance(a: Fingerprint, b: Fingerprint) -> u32 {
    hamming(a.dhash, b.dhash).min(hamming(a.phash, b.phash))
}

fn is_duplicate(a: Fingerprint, b: Fingerprint, limit: u32) -> bool {
    duplicate_distance(a, b) <= limit
}

fn is_maybe(a: Fingerprint, b: Fingerprint, duplicate_limit: u32, maybe_limit: u32) -> bool {
    !is_duplicate(a, b, duplicate_limit) && maybe_distance(a, b) <= maybe_limit
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
            if is_duplicate(
                images[i].fingerprint,
                images[j].fingerprint,
                duplicate_limit,
            ) {
                let dist = duplicate_distance(images[i].fingerprint, images[j].fingerprint);
                dup_edges.push((i, j, dist));
                union(&mut parent, i, j);
            }
        }
    }

    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        buckets[find(&mut parent, i)].push(i);
    }

    let mut in_duplicate = vec![false; n];
    let mut groups = Vec::new();
    for members in buckets {
        if members.len() < 2 {
            continue;
        }
        for &i in &members {
            in_duplicate[i] = true;
        }
        let mut best = HASH_BITS;
        for &(a, b, dist) in &dup_edges {
            if members.contains(&a) && members.contains(&b) {
                best = best.min(dist);
            }
        }
        let mut hashes: Vec<String> = members.iter().map(|&i| images[i].hash.clone()).collect();
        hashes.sort();
        groups.push(SimilarGroup {
            kind: GroupKind::Duplicate,
            hashes,
            percent: percent_from_distance(best),
        });
    }

    for i in 0..n {
        if in_duplicate[i] {
            continue;
        }
        for j in (i + 1)..n {
            if in_duplicate[j] {
                continue;
            }
            if !is_maybe(
                images[i].fingerprint,
                images[j].fingerprint,
                duplicate_limit,
                maybe_limit,
            ) {
                continue;
            }
            let dist = maybe_distance(images[i].fingerprint, images[j].fingerprint);
            let mut hashes = vec![images[i].hash.clone(), images[j].hash.clone()];
            hashes.sort();
            groups.push(SimilarGroup {
                kind: GroupKind::Maybe,
                hashes,
                percent: percent_from_distance(dist),
            });
        }
    }

    groups.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.hashes.cmp(&b.hashes))
    });
    groups
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

    #[test]
    fn identical_pngs_match() {
        let bytes = png_bytes(&patterned(1));
        let a = fp(&bytes);
        let b = fp(&bytes);
        assert_eq!(a, b);
        assert_eq!(duplicate_distance(a, b), 0);
    }

    #[test]
    fn jpeg_recompress_stays_within_duplicate_limit() {
        let image = patterned(3);
        let high = fp(&jpeg_bytes(&image, 90));
        let low = fp(&jpeg_bytes(&image, 40));
        assert!(
            duplicate_distance(high, low) <= 8,
            "distance {}",
            duplicate_distance(high, low)
        );
    }

    #[test]
    fn unrelated_patterns_are_far() {
        let a = fp(&png_bytes(&patterned(1)));
        let b = fp(&png_bytes(&patterned(200)));
        assert!(
            duplicate_distance(a, b) > 16,
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
    fn cluster_links_high_confidence_and_pairs_leftovers() {
        let dup = Fingerprint {
            dhash: 0x1111,
            phash: 0x1111,
        };
        let dup_close = Fingerprint {
            dhash: 0x1113,
            phash: 0x1110,
        };
        let leftover_a = Fingerprint {
            dhash: 0xAAAA_AAAA_AAAA_AAAA,
            phash: 0x5555_5555_5555_5555,
        };
        let leftover_b = Fingerprint {
            dhash: 0xAAAA_AAAA_AAAA_AAAB,
            phash: 0x0,
        };
        let outsider = Fingerprint {
            dhash: 0xFFFF_0000_FFFF_0000,
            phash: 0x00FF_00FF_00FF_00FF,
        };

        let images = vec![
            HashedImage {
                hash: "a".into(),
                fingerprint: dup,
            },
            HashedImage {
                hash: "b".into(),
                fingerprint: dup_close,
            },
            HashedImage {
                hash: "c".into(),
                fingerprint: leftover_a,
            },
            HashedImage {
                hash: "d".into(),
                fingerprint: leftover_b,
            },
            HashedImage {
                hash: "e".into(),
                fingerprint: outsider,
            },
        ];

        let groups = cluster(&images, 8, 16);
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
    fn leftover_does_not_pair_with_duplicate_member() {
        let dup = Fingerprint { dhash: 1, phash: 1 };
        let dup2 = Fingerprint { dhash: 1, phash: 3 };
        let leftover = Fingerprint {
            dhash: 1,
            phash: u64::MAX,
        };
        let images = vec![
            HashedImage {
                hash: "a".into(),
                fingerprint: dup,
            },
            HashedImage {
                hash: "b".into(),
                fingerprint: dup2,
            },
            HashedImage {
                hash: "c".into(),
                fingerprint: leftover,
            },
        ];
        let groups = cluster(&images, 8, 16);
        assert!(groups.iter().all(|g| g.kind != GroupKind::Maybe));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hashes, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn percent_uses_hamming_over_64_bits() {
        assert_eq!(percent_from_distance(0), 100);
        assert_eq!(percent_from_distance(8), 87);
        assert_eq!(percent_from_distance(16), 75);
        assert_eq!(percent_from_distance(64), 0);
    }

    #[test]
    fn distance_from_percent_matches_title_rounding() {
        assert_eq!(distance_from_percent(100), 0);
        assert_eq!(distance_from_percent(87), 8);
        assert_eq!(distance_from_percent(75), 16);
        assert_eq!(distance_from_percent(88), 7);
        assert_eq!(
            u32::from(percent_from_distance(distance_from_percent(90))),
            90
        );
    }
}
