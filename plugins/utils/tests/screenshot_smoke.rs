// 所有场景放在同一个 #[tokio::test] 里串行跑：全局 ScreenshotManager 是进程级
// 单例，若分到多个 #[tokio::test]，首个用例的 runtime 退出时会 abort 掉 chromiumoxide
// 的 handler task，导致共享浏览器变僵尸，后续用例拿到失效连接并误触发浏览器重启。
//
// 注意：用例结束后浏览器会残留为孤儿进程并占用 chromiumoxide 的固定 profile 目录，
// 再次运行时若提示 SingletonLock 冲突，先清理残留的 chromiumoxide-runner 进程。
#[tokio::test]
#[ignore = "需要本机安装 Chromium/Chrome"]
async fn screenshot_smoke() {
    // 全页截图 + 静态元素裁剪。
    let html = r#"<!doctype html><html><body><article style="width:320px;height:120px;background:#fff;color:#111">smoke</article></body></html>"#;

    let full = utils::screenshot(html, utils::ScreenshotOptions::default())
        .await
        .expect("full screenshot");
    let article = utils::screenshot(
        html,
        utils::ScreenshotOptions::new()
            .with_selector("article")
            .with_wait_selectors(&["article"]),
    )
    .await
    .expect("selector screenshot");

    assert!(full.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(article.starts_with(b"\x89PNG\r\n\x1a\n"));

    // 等待 JS 延迟注入的选择器：`.ready` 在页面加载 500ms 后才出现，
    // 轮询必须真正等它出现才能截到图。裁剪目标直接选 `#target.ready`：
    // 若等待逻辑失效，find_element 会失败使本用例报错。
    let delayed_html = r#"<!doctype html><html><body><div id="target" style="width:120px;height:120px;background:#eef">wait</div>
<script>setTimeout(() => document.getElementById('target').classList.add('ready'), 500);</script></body></html>"#;

    let delayed_png = utils::screenshot(
        delayed_html,
        utils::ScreenshotOptions::new()
            .with_selector("#target.ready")
            .with_wait_selectors(&["#target.ready"]),
    )
    .await
    .expect("should wait for the delayed element");
    assert!(delayed_png.starts_with(b"\x89PNG\r\n\x1a\n"));

    // 等待永不出现的选择器：必须在 `SCREENSHOT_WAIT_TIMEOUT` 内报出元素等待超时。
    let plain_html = r#"<!doctype html><html><body><div id="x">hi</div></body></html>"#;

    let err = utils::screenshot(
        plain_html,
        utils::ScreenshotOptions::new()
            .with_selector("#x")
            .with_wait_selectors(&["#never-inserted"]),
    )
    .await
    .expect_err("missing wait selector must fail");
    assert!(
        err.to_string().contains("#never-inserted"),
        "unexpected error: {err:#}"
    );
}
