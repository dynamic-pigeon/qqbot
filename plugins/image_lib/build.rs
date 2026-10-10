// SIFT::create 自 OpenCV 4.7 起追加 enable_precise_upscale 尾参，similar.rs
// 按这里下发的 cfg 分新旧两个签名。pkg-config 探测失败时不发 cfg，按 4.6
// 旧签名编译（CI 与部署 chroot 的口径）。
fn main() {
    println!("cargo:rustc-check-cfg=cfg(opencv_ge_4_7)");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");
    let output = match std::process::Command::new("pkg-config")
        .args(["--modversion", "opencv4"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return,
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version.trim().split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return;
    };
    let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
        return;
    };
    if major == 4 && minor >= 7 {
        println!("cargo:rustc-cfg=opencv_ge_4_7");
    }
}
