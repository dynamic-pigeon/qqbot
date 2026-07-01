use std::path::PathBuf;

/// 生成把 KaTeX woff2 字体内联成 data URI 的 CSS。
///
/// 原 `katex.min.css` 通过 `url(fonts/KaTeX_*.woff2)` 引用外部字体；在 CSP
/// `font-src 'none'` 或 `about:blank` 相对路径下都无法加载，导致数学公式
/// 渲染为缺失字形。构建时把 woff2 字体 base64 进 CSS，使字体自包含，只依赖
/// `font-src 'self' data:`。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").ok_or("OUT_DIR must be set")?,
    );
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("CARGO_MANIFEST_DIR must be set")?,
    );

    let assets_dir = manifest_dir.join("src/markdown/assets");
    let fonts_dir = assets_dir.join("fonts");
    let css_path = assets_dir.join("katex.min.css");
    let output_path = out_dir.join("katex.inline.css");

    println!("cargo:rerun-if-changed={}", css_path.display());
    println!("cargo:rerun-if-changed={}", fonts_dir.display());

    let css = std::fs::read_to_string(&css_path)
        .map_err(|e| format!("failed to read {}: {e}", css_path.display()))?;

    let mut inlined = css;
    for entry in std::fs::read_dir(&fonts_dir)
        .map_err(|e| format!("failed to read {}: {e}", fonts_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .ok_or("font path has no file name")?
            .to_string_lossy();
        if !name.ends_with(".woff2") {
            continue;
        }

        let data = std::fs::read(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let b64 = base64_encode(&data);
        let placeholder = format!("url(fonts/{name})");
        let data_url = format!("url(data:font/woff2;base64,{b64})");

        if inlined.contains(&placeholder) {
            inlined = inlined.replace(&placeholder, &data_url);

            // 同一字体族的 woff/ttf 回退在 woff2 已内联后不会再被用到，
            // 移除它们可避免残留的外部 URL 被 CSP / about:blank 拦截。
            let stem = name
                .strip_suffix(".woff2")
                .ok_or("font name must end with .woff2")?;
            let woff_ref = format!(",url(fonts/{stem}.woff) format(\"woff\")");
            let ttf_ref = format!(",url(fonts/{stem}.ttf) format(\"truetype\")");
            inlined = inlined.replace(&woff_ref, "");
            inlined = inlined.replace(&ttf_ref, "");
        }
    }

    std::fs::write(&output_path, inlined)
        .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;

    Ok(())
}

/// 无 padding 变体的标准 base64（CSS data URI 不需要 padding）。
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // 标准 base64 每 3 字节输入产生 4 字节输出，不足 3 字节也补齐到 4 字节。
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    let chunks = input.chunks_exact(3);
    let remainder = chunks.remainder();
    for chunk in chunks {
        let n = u32::from_be_bytes([0, chunk[0], chunk[1], chunk[2]]);
        out.push(TABLE[((n >> 18) & 0x3f) as usize]);
        out.push(TABLE[((n >> 12) & 0x3f) as usize]);
        out.push(TABLE[((n >> 6) & 0x3f) as usize]);
        out.push(TABLE[(n & 0x3f) as usize]);
    }
    match remainder.len() {
        0 => {}
        1 => {
            let n = (remainder[0] as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize]);
            out.push(TABLE[((n >> 12) & 0x3f) as usize]);
            out.push(b'=');
            out.push(b'=');
        }
        2 => {
            let n = ((remainder[0] as u32) << 16) | ((remainder[1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize]);
            out.push(TABLE[((n >> 12) & 0x3f) as usize]);
            out.push(TABLE[((n >> 6) & 0x3f) as usize]);
            out.push(b'=');
        }
        _ => unreachable!(),
    }
    // base64 表只含 ASCII，因此 from_utf8 不会失败。
    String::from_utf8(out).expect("base64 table is ASCII")
}
