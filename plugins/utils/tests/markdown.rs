#![cfg(feature = "markdown")]

use utils::md_to_html;

#[test]
fn md_to_html_embeds_document_shell_and_assets() {
    let html = md_to_html("你好 **世界**");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<article class=\"markdown-body\">"));
    assert!(html.contains("你好"));
    assert!(html.contains("<strong>"));
    assert!(html.contains(".markdown-body"));
    assert!(html.contains("hljs.highlightAll"));
    assert!(html.contains("katex.render"));
    assert!(html.ends_with("</html>"));
}

/// Chromium 截图要加载 KaTeX 字体；CSP 允许 `data:`，构建时把 woff2 内联成 data URI。
#[test]
fn html_inlines_katex_fonts_for_csp() {
    let html = md_to_html("$E=mc^2$");
    assert!(!html.contains("font-src 'none'"));
    assert!(html.contains("font-src 'self' data:"));
    assert!(!html.contains("url(fonts/KaTeX_"));
    assert!(html.contains("data:font/woff2;base64,"));
}
