use anyhow::Result;
use askama::Template;
use pulldown_cmark::Options;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use tracing::error;

/// 复用的 Markdown 解析选项，避免每次调用都重新构造。
static MARKDOWN_OPTIONS: LazyLock<Options> = LazyLock::new(|| {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    options
});

// sanitize 白名单集合合计约 4KB 常驻内存，相比单次截图启动的浏览器进程
// （数百 MB）是噪声级别，不值得为它们做按需构建与空闲回收，直接静态化。
static ALLOWED_TAGS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // 基础结构
        "p",
        "br",
        "hr",
        "div",
        "span",
        // 标题
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        // 文本格式
        "strong",
        "em",
        "b",
        "i",
        "u",
        "s",
        "del",
        "ins",
        "mark",
        "sub",
        "sup",
        "code",
        "pre",
        "kbd",
        "samp",
        "var",
        // 列表
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        // 表格
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "th",
        "td",
        "caption",
        "col",
        "colgroup",
        // 引用
        "blockquote",
        "q",
        "cite",
        // 细节
        "details",
        "summary",
        // 链接（仅保留文本，href 会被 scheme 过滤）
        "a",
        // 其他语义标签
        "abbr",
        "bdi",
        "bdo",
        "dfn",
        "small",
        "time",
        "wbr",
    ]
    .into_iter()
    .collect()
});

static GENERIC_ATTRS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["class", "id", "title", "dir", "lang"]
        .into_iter()
        .collect()
});

/// 各标签额外允许的属性；目前只有 `a` 的 `href`。
static TAG_ATTRIBUTES: LazyLock<HashMap<&'static str, HashSet<&'static str>>> =
    LazyLock::new(|| HashMap::from([("a", HashSet::from(["href"]))]));

static URL_SCHEMES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["http", "https", "mailto"].into_iter().collect());

#[derive(Template)]
#[template(path = "markdown.html")]
struct MarkdownTemplate {
    katex_css: &'static str,
    katex_js: &'static str,
    github_md_css: &'static str,
    highlight_css: &'static str,
    highlight_js: &'static str,
    body: String,
}

pub async fn md_to_img(md: &str) -> Result<Vec<u8>> {
    let html = md_to_html(md);

    // 等模板里的 `.finish` 标志出现再截图，保证 KaTeX / highlight.js 渲染完成。
    let png_data = match crate::screen_shot::screenshot(
        &html,
        crate::screen_shot::ScreenshotOptions::new()
            .with_selector("article.markdown-body")
            .with_wait_selectors(&["div.finish"]),
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            error!("{}", err);
            return Err(err);
        }
    };

    Ok(png_data)
}

pub fn md_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, *MARKDOWN_OPTIONS);

    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, parser);

    // pulldown-cmark 默认透传 raw HTML（包括 `<script>`、`<iframe>`、on-event handler、
    // `javascript:` URI 等）。这个 body 会被 askama 模板以 `{{ body|safe }}` 原样嵌入，
    // 最终送进 Chromium headless 渲染。任何用户提交的 markdown 都可以借此发起任意
    // HTTP GET（`<img src=http://169.254.169.254/...>`）或执行任意 JS（`<script>`）。
    // 必须先 sanitize 再嵌入。
    let body = sanitize_html(&body);

    MarkdownTemplate {
        // woff2 字体会被 build.rs 内联成 data URI，避免 CSP / about:blank 相对路径问题。
        katex_css: include_str!(concat!(env!("OUT_DIR"), "/katex.inline.css")),
        katex_js: include_str!("assets/katex.min.js"),
        github_md_css: include_str!("assets/github_md_light.css"),
        highlight_css: include_str!("assets/highlight_github_light.css"),
        highlight_js: include_str!("assets/highlight.js"),
        body,
    }
    .render()
    .unwrap()
}

/// 使用基于 HTML5 spec 的白名单 sanitizer 清理用户提交的 Markdown 渲染结果。
///
/// 策略：
/// - 只允许纯文本格式和页面结构标签；
/// - 禁止 `<img>` / `<video>` / `<audio>` / `<source>` / `<iframe>` / `<object>` /
///   `<embed>` / `<form>` / `<link>` / `<meta>` / `<base>` / `<style>` / `<script>` 等
///   能加载远程资源或执行代码的标签；
/// - 只允许 `http` / `https` / `mailto` 三种 URL scheme；
/// - 全局移除 `on*` 事件处理器；
/// - 为保留的 `<a>` 自动添加 `rel="nofollow noopener noreferrer"`。
///
/// 该 sanitizer 与 CSP 头配合使用：即使某处被绕过，Chromium 也被限制在最小权限集。
fn sanitize_html(input: &str) -> String {
    // ammonia 的 setter 是 move 语义，这里从静态白名单 clone 一份；
    // clone 直接复制 hashbrown 内部表，不重新计算 hash，比每次重建快。
    ammonia::Builder::default()
        .tags(ALLOWED_TAGS.clone())
        .generic_attributes(GENERIC_ATTRS.clone())
        .tag_attributes(TAG_ATTRIBUTES.clone())
        .url_schemes(URL_SCHEMES.clone())
        .link_rel(Some("nofollow noopener noreferrer"))
        .clean(input)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_to_html_keeps_safe_inline_tags() {
        let html = md_to_html("<b>bold</b>");
        assert!(html.contains("<b>bold</b>"));
    }

    #[test]
    fn sanitize_strips_dangerous_markup() {
        // 覆盖白名单策略：禁脚本、禁远程资源标签、禁 javascript: 和 on*。
        for (input, forbidden) in [
            ("hello<script>alert(1)</script>world", "<script"),
            (
                r#"<img src="http://169.254.169.254/latest/meta-data/">"#,
                "<img",
            ),
            (r#"<a href="javascript:alert(1)">click</a>"#, "javascript:"),
            (
                r#"<a href="https://ok.com" onclick="alert(1)">click</a>"#,
                "onclick",
            ),
        ] {
            let out = sanitize_html(input);
            assert!(
                !out.to_ascii_lowercase().contains(forbidden),
                "expected {forbidden:?} stripped from {input:?}, got {out}"
            );
        }
        let script = sanitize_html("hello<script>alert(1)</script>world");
        assert!(script.contains("hello") && script.contains("world"));
        let link = sanitize_html(r#"<a href="https://ok.com" onclick="alert(1)">click</a>"#);
        assert!(link.contains("href=\"https://ok.com\""));
    }

    #[test]
    fn sanitize_preserves_safe_markup() {
        let formatted = sanitize_html("<p>hello <strong>world</strong></p>");
        assert!(formatted.contains("<strong>world</strong>"));
        assert!(formatted.contains("<p>"));
        assert!(
            sanitize_html(r#"<a href="https://example.com">x</a>"#).contains("https://example.com")
        );
        assert!(sanitize_html(r#"<a href="mailto:a@b.com">x</a>"#).contains("mailto:a@b.com"));
    }
}
