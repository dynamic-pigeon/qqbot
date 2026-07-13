#[tokio::test]
#[ignore = "需要本机安装 Chromium/Chrome"]
async fn renders_full_page_and_selector_to_png() {
    let html = r#"<!doctype html><html><body><article style="width:320px;height:120px;background:#fff;color:#111">smoke</article></body></html>"#;

    let full = utils::screenshot(html, None)
        .await
        .expect("full screenshot");
    let article = utils::screenshot(html, Some("article"))
        .await
        .expect("selector screenshot");

    assert!(full.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(article.starts_with(b"\x89PNG\r\n\x1a\n"));
}
