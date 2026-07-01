use pulldown_cmark::Options;
use utils::md_to_html;

/// 验证 md_to_html 返回完整的 HTML 文档结构
fn assert_valid_html_structure(html: &str) {
    assert!(
        html.starts_with("<!doctype html>"),
        "should start with doctype"
    );
    assert!(html.contains("<html>"), "should contain <html>");
    assert!(html.contains("<head>"), "should contain <head>");
    assert!(
        html.contains("<meta charset=\"UTF-8\">"),
        "should contain UTF-8 meta"
    );
    assert!(html.contains("<body>"), "should contain <body>");
    assert!(
        html.contains("<article class=\"markdown-body\">"),
        "should contain markdown-body article"
    );
    assert!(html.ends_with("</html>"), "should end with </html>");
}

/// 验证 CSS/JS 资源被正确嵌入
fn assert_assets_embedded(html: &str) {
    assert!(html.contains("<style>"), "should contain inline <style>");
    assert!(
        html.contains(".markdown-body"),
        "should contain markdown-body CSS"
    );
    assert!(html.contains("<script>"), "should contain inline <script>");
    assert!(
        html.contains("hljs.highlightAll"),
        "should contain highlight.js init"
    );
    assert!(
        html.contains("katex.render"),
        "should contain KaTeX render logic"
    );
}

#[test]
fn test_empty_input() {
    let html = md_to_html("");
    assert_valid_html_structure(&html);
    assert_assets_embedded(&html);
}

#[test]
fn test_headings() {
    let html = md_to_html("# H1\n## H2\n### H3");
    assert!(html.contains("<h1>"), "should render h1");
    assert!(html.contains("H1"), "should contain heading text");
    assert!(html.contains("<h2>"), "should render h2");
    assert!(html.contains("<h3>"), "should render h3");
}

#[test]
fn test_code_block() {
    let html = md_to_html("```rust\nfn main() {}\n```");
    // code blocks produce <pre><code> which triggers the maxWidth:720px logic
    assert!(html.contains("<pre>"), "should contain pre tag");
    assert!(html.contains("<code"), "should contain code tag");
}

#[test]
fn test_table() {
    let md = "\
| a | b |
|---|---|
| 1 | 2 |
";
    let html = md_to_html(md);
    assert!(html.contains("<table>"), "should render table");
    assert!(html.contains("<th>"), "should contain table header");
    assert!(html.contains("<td>"), "should contain table cell");
}

#[test]
fn test_math_inline() {
    let html = md_to_html("$E=mc^2$");
    // pulldown-cmark with ENABLE_MATH wraps inline math in class="math math-inline"
    assert!(
        html.contains("math-inline") || html.contains("math inline"),
        "should contain math class for inline formula"
    );
}

#[test]
fn test_math_display() {
    let html = md_to_html("$$\nE=mc^2\n$$");
    // pulldown-cmark with ENABLE_MATH wraps display math in class="math math-display"
    assert!(
        html.contains("math-display") || html.contains("math display"),
        "should contain math class for display formula"
    );
}

#[test]
fn test_link() {
    let html = md_to_html("[click](https://example.com)");
    assert!(
        html.contains("<a href=\"https://example.com\""),
        "should render link"
    );
    assert!(html.contains("click"), "should contain link text");
}

#[test]
fn test_chinese_text() {
    let html = md_to_html("你好世界");
    assert_valid_html_structure(&html);
    assert!(
        html.contains("你好世界"),
        "should preserve Chinese characters"
    );
}

#[test]
fn test_mixed_content() {
    let md = "\
# 标题

这是一段**重要**的文字，包含 `代码` 和 [链接](https://example.com)。

| 列A | 列B |
|-----|-----|
| 值1 | 值2 |

- 列表项1
- 列表项2
";
    let html = md_to_html(md);
    assert_valid_html_structure(&html);
    assert_assets_embedded(&html);
    assert!(html.contains("<h1>"), "should have h1");
    assert!(html.contains("<strong>"), "should have bold");
    assert!(html.contains("<code>"), "should have inline code");
    assert!(html.contains("<a href="), "should have link");
    assert!(html.contains("<table>"), "should have table");
    assert!(html.contains("<ul>"), "should have unordered list");
}

/// 验证模板渲染的结果与通过 pulldown-cmark 直接渲染的 body 一致
#[test]
fn test_body_preserved_in_template() {
    let md = "## test body";
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    let parser = pulldown_cmark::Parser::new_ext(md, options);

    let mut expected_body = String::new();
    pulldown_cmark::html::push_html(&mut expected_body, parser);

    let html = md_to_html(md);
    assert!(
        html.contains(&expected_body),
        "template should embed the rendered markdown body unchanged"
    );
}

/// 回归 finding-1: CSP `font-src 'none'` 曾拦截 KaTeX 字体，
/// 导致 `$E=mc^2$` 这类数学公式在 Chromium 截图里全是缺失字形的方框。
///
/// 修复后：
///   1. CSP 必须允许 data: 字体（font-src 'self' data:），不能是 'none';
///   2. 内联的 KaTeX CSS 必须把 woff2 字体替换成 data URI，不再引用外部文件;
///   3. 仓库 assets/fonts/ 必须存在供构建脚本使用。
#[test]
fn finding1_csp_allows_inlined_katex_fonts() {
    // 1. 渲染一条带数学公式的 markdown
    let html = md_to_html("$E=mc^2$");

    // 2. CSP 必须允许 data: 字体，而不是 'none'
    assert!(
        !html.contains("font-src 'none'"),
        "CSP 仍然完全禁止字体（font-src 'none'），内联字体也无法加载。"
    );
    assert!(
        html.contains("font-src 'self' data:"),
        "CSP 必须允许 data: URI 字体，才能加载内联的 KaTeX 字体。"
    );

    // 3. 内联的 KaTeX CSS 不再引用任何外部字体文件
    assert!(
        !html.contains("url(fonts/KaTeX_"),
        "内联 KaTeX CSS 仍引用外部字体文件，CSP 会拦截。"
    );

    // 4. 仓库 assets/ 目录下存在 fonts/ 子目录（构建脚本用其生成内联 CSS）
    let assets_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/markdown/assets");
    assert!(
        assets_dir.join("fonts").exists(),
        "assets/fonts/ 必须存在，供 build.rs 把字体 base64 进 CSS。"
    );

    // 5. 双重确认：几族常用字体都被内联为 data URI
    let katex_css = std::fs::read_to_string(assets_dir.join("katex.min.css"))
        .expect("katex.min.css 必须在 assets/ 下");
    for family in ["KaTeX_AMS", "KaTeX_Main", "KaTeX_Caligraphic", "KaTeX_Size"] {
        assert!(
            katex_css.contains(&format!("url(fonts/{family}")),
            "原始 katex.min.css 应引用 {family} 字体族"
        );
    }
    // 渲染后的 HTML 里这些字体族应以 data:font/woff2;base64, 形式出现
    assert!(
        html.contains("data:font/woff2;base64,"),
        "渲染输出应包含 base64 内联的 woff2 字体"
    );
}
