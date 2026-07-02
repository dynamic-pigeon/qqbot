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

    let png_data = match crate::screen_shot::screenshot(
        html.into(),
        Some(std::borrow::Cow::Borrowed("article.markdown-body")),
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
    let allowed_tags: HashSet<&str> = [
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
    .iter()
    .copied()
    .collect();

    let generic_attrs: HashSet<&str> = ["class", "id", "title", "dir", "lang"]
        .iter()
        .copied()
        .collect();

    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    let a_attrs: HashSet<&str> = ["href"].iter().copied().collect();
    tag_attributes.insert("a", a_attrs);

    let url_schemes: HashSet<&str> = ["http", "https", "mailto"].iter().copied().collect();

    ammonia::Builder::default()
        .tags(allowed_tags)
        .generic_attributes(generic_attrs)
        .tag_attributes(tag_attributes)
        .url_schemes(url_schemes)
        .link_rel(Some("nofollow noopener noreferrer"))
        .clean(input)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_special_characters_preserved() {
        let html = md_to_html("<b>bold</b>");
        assert!(
            html.contains("<b>bold</b>"),
            "safe inline HTML like <b> should be preserved"
        );
        let body_sanitized = sanitize_html("<script>alert(1)</script>hello");
        assert!(
            !body_sanitized.contains("<script") && !body_sanitized.contains("alert(1)"),
            "body script must be stripped, got: {body_sanitized}"
        );
    }

    #[test]
    fn sanitize_strips_script_block() {
        let out = sanitize_html("hello<script>alert(1)</script>world");
        assert!(!out.contains("<script"));
        assert!(!out.contains("alert"));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn sanitize_strips_iframe_and_object() {
        assert!(!sanitize_html("a<iframe src='http://evil'></iframe>b").contains("<iframe"));
        assert!(!sanitize_html("a<object></object>b").contains("<object"));
        assert!(!sanitize_html("a<embed src='x'>b").contains("<embed"));
        assert!(!sanitize_html("a<form action='x'></form>b").contains("<form"));
    }

    #[test]
    fn sanitize_strips_link_meta_base_style() {
        assert!(!sanitize_html("<link rel='stylesheet' href='http://evil/x'>").contains("<link"));
        assert!(
            !sanitize_html("<meta http-equiv='refresh' content='0;url=http://evil'>")
                .contains("<meta")
        );
        assert!(!sanitize_html("<base href='http://evil/'>").contains("<base"));
        assert!(!sanitize_html("<style>body{display:none}</style>").contains("<style"));
    }

    #[test]
    fn sanitize_strips_on_event_attrs() {
        let out = sanitize_html(r#"<a href="https://ok.com" onclick="alert(1)">click</a>"#);
        assert!(!out.contains("onclick"));
        assert!(!out.contains("alert(1)"));
        // href 应保留
        assert!(out.contains("href=\"https://ok.com\""));
    }

    #[test]
    fn sanitize_strips_on_event_attrs_single_quote_and_unquoted() {
        let out = sanitize_html(r#"<img src=x onerror='alert(1)' alt=y>"#);
        assert!(!out.contains("onerror"));
        assert!(!out.contains("alert"));
        let out2 = sanitize_html(r#"<img src=x onload=alert(1) alt=y>"#);
        assert!(!out2.contains("onload"));
    }

    #[test]
    fn sanitize_strips_on_event_attrs_with_slash_separator() {
        let out = sanitize_html(r#"<img/src=x/onerror=alert(1)>"#);
        assert!(!out.to_ascii_lowercase().contains("onerror"));
        assert!(!out.contains("alert(1)"));
    }

    #[test]
    fn sanitize_removes_dangerous_uri_schemes() {
        // ammonia 会移除非法 href 属性而不是替换成 about:blank
        let out = sanitize_html(r#"<a href="javascript:alert(1)">click</a>"#);
        assert!(!out.to_ascii_lowercase().contains("javascript:"));
        assert!(out.contains("click"));

        let out2 = sanitize_html(r#"<img src="javascript:alert(1)">"#);
        assert!(!out2.to_ascii_lowercase().contains("javascript:"));
        // img 标签本身也不在白名单中
        assert!(!out2.contains("<img"));

        let out3 = sanitize_html(r#"<a href="vbscript:msgbox(1)">click</a>"#);
        assert!(!out3.to_ascii_lowercase().contains("vbscript:"));

        let out4 = sanitize_html(r#"<a href="data:text/html,<script>alert(1)</script>">click</a>"#);
        assert!(!out4.to_ascii_lowercase().contains("data:"));
    }

    #[test]
    fn sanitize_removes_unquoted_dangerous_uri() {
        let out = sanitize_html(r#"<a href=javascript:alert(1)>click</a>"#);
        assert!(!out.to_ascii_lowercase().contains("javascript:"));
        assert!(out.contains("click"));

        let out3 = sanitize_html(r#"<a/href=javascript:alert(1)>x</a>"#);
        assert!(!out3.to_ascii_lowercase().contains("javascript:"));

        let out4 = sanitize_html(r#"<a href=JaVaScRiPt:alert(1)>x</a>"#);
        assert!(!out4.to_ascii_lowercase().contains("javascript:"));

        let out5 = sanitize_html(r#"<a href=vbscript:msgbox(1)>x</a>"#);
        assert!(!out5.to_ascii_lowercase().contains("vbscript:"));

        let out6 = sanitize_html(r#"<a href=data:text/html,<script>alert(1)</script>>x</a>"#);
        assert!(!out6.to_ascii_lowercase().contains("data:"));

        let out7 = sanitize_html(
            r#"<a href=data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==>x</a>"#,
        );
        assert!(!out7.to_ascii_lowercase().contains("data:"));

        // 正常 bareword URL 不应被误伤
        let out8 = sanitize_html(r#"<a href=https://example.com>x</a>"#);
        assert!(out8.contains("https://example.com"));
        let out9 = sanitize_html(r#"<a href=mailto:a@b.com>x</a>"#);
        assert!(out9.contains("mailto:a@b.com"));
    }

    #[test]
    fn sanitize_preserves_safe_links() {
        assert!(
            sanitize_html(r#"<a href="https://example.com">x</a>"#).contains("https://example.com")
        );
        assert!(sanitize_html(r#"<a href="mailto:a@b.com">x</a>"#).contains("mailto:a@b.com"));
    }

    #[test]
    fn sanitize_preserves_inline_formatting() {
        let html = sanitize_html("<p>hello <strong>world</strong></p>");
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("<p>"));
    }

    #[test]
    fn sanitize_removes_remote_resource_tags() {
        // img 不在白名单中，可防止 SSRF
        assert!(
            !sanitize_html(r#"<img src="http://169.254.169.254/latest/meta-data/">"#)
                .contains("<img")
        );
        assert!(!sanitize_html(r#"<video src="http://evil"></video>"#).contains("<video"));
        assert!(!sanitize_html(r#"<audio src="http://evil"></audio>"#).contains("<audio"));
        assert!(!sanitize_html(r#"<source src="http://evil">"#).contains("<source"));
    }
}
