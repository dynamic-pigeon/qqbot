use anyhow::Result;
use askama::Template;
use pulldown_cmark::Options;
use tracing::error;

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

fn md_to_html(md: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    let parser = pulldown_cmark::Parser::new_ext(md, options);

    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, parser);

    MarkdownTemplate {
        katex_css: include_str!("html/katex.min.css"),
        katex_js: include_str!("html/katex.min.js"),
        github_md_css: include_str!("html/github_md_light.css"),
        highlight_css: include_str!("html/highlight_github_light.css"),
        highlight_js: include_str!("html/highlight.js"),
        body,
    }
    .render()
    .unwrap()
}

#[cfg(test)]
mod tests {

    use super::*;

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
    fn test_plain_text() {
        let html = md_to_html("hello world");
        assert_valid_html_structure(&html);
        assert!(
            html.contains("hello world"),
            "should contain the input text"
        );
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
    fn test_bold_and_italic() {
        let html = md_to_html("**bold** and *italic*");
        assert!(
            html.contains("<strong>bold</strong>") || html.contains("<strong>bold</strong>"),
            "should render bold"
        );
        assert!(html.contains("<em>italic</em>"), "should render italic");
    }

    #[test]
    fn test_code_inline() {
        let html = md_to_html("use `println!` macro");
        assert!(html.contains("<code>"), "should contain inline code tag");
        assert!(html.contains("println!"), "should contain code text");
    }

    #[test]
    fn test_code_block() {
        let html = md_to_html("```rust\nfn main() {}\n```");
        // code blocks produce <pre><code> which triggers the maxWidth:720px logic
        assert!(html.contains("<pre>"), "should contain pre tag");
        assert!(html.contains("<code"), "should contain code tag");
    }

    #[test]
    fn test_strikethrough() {
        let html = md_to_html("~~deleted~~");
        // pulldown-cmark with ENABLE_STRIKETHROUGH should render <del> or <s>
        assert!(
            html.contains("<del>") || html.contains("<s>"),
            "should render strikethrough"
        );
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
    fn test_footnote() {
        let html = md_to_html("text[^1]\n\n[^1]: footnote content");
        // ENABLE_FOOTNOTES should render footnote references
        assert!(
            html.contains("footnote"),
            "should contain footnote references"
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
    fn test_unordered_list() {
        let html = md_to_html("* item1\n* item2");
        assert!(html.contains("<ul>"), "should render unordered list");
        assert!(html.contains("<li>"), "should contain list items");
    }

    #[test]
    fn test_ordered_list() {
        let html = md_to_html("1. first\n2. second");
        assert!(html.contains("<ol>"), "should render ordered list");
        assert!(html.contains("<li>"), "should contain list items");
    }

    #[test]
    fn test_blockquote() {
        let html = md_to_html("> quoted text");
        assert!(html.contains("<blockquote>"), "should render blockquote");
    }

    #[test]
    fn test_horizontal_rule() {
        let html = md_to_html("---");
        assert!(html.contains("<hr"), "should render horizontal rule");
    }

    #[test]
    fn test_special_characters_preserved() {
        // pulldown-cmark passes raw HTML through by default (inline HTML is valid markdown).
        // The markdown source comes from QQ messages, which already filter script tags upstream.
        let html = md_to_html("<b>bold</b>");
        assert!(
            html.contains("<b>bold</b>"),
            "raw HTML should be passed through by pulldown-cmark"
        );
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
        let mut options = pulldown_cmark::Options::empty();
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
}
