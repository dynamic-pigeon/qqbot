use anyhow::Result;
use html::END;
use kovi::log::error;
use pulldown_cmark::Options;

mod html;

pub async fn md_to_img(md: &str) -> Result<Vec<u8>> {
    let html = md_to_html(md).await;

    let png_data = match crate::screen_shot::screenshot(&html, Some("article.markdown-body")).await
    {
        Ok(v) => v,
        Err(err) => {
            error!("{}", err);
            return Err(err);
        }
    };

    Ok(png_data)
}

async fn md_to_html(md: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    let parser = pulldown_cmark::Parser::new_ext(md, options);

    let mut html_output = String::new();
    html_output.push_str(html::HTML_START_NEXT_IS_MD_CSS);

    html_output.push_str(html::GITHUB_MARKDOWN_LIGHT_NEXT_IS_HTML2);

    html_output.push_str(html::HTML_2_NEXT_IS_HIGHLIGHT_CSS);

    html_output.push_str(html::HIGH_LIGHT_LIGHT_CSS_NEXT_IS_HTML3);

    html_output.push_str(html::HTML_3_NEXT_IS_MD_BODY_AND_THEN_IS_HTML4);
    pulldown_cmark::html::push_html(&mut html_output, parser);
    html_output.push_str(html::HTML_4_NEXT_IS_HIGH_LIGHT_JS);
    html_output.push_str(html::HIGH_LIGHT_JS_NEXT_IS_HTML_END);
    html_output.push_str(html::HTML_END);
    html_output.push_str(&format!("<script>{}</script>", html::HTML_SCRIPT));
    html_output.push_str(END);

    html_output
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    use super::*;
    #[tokio::test]
    async fn test_screenshot() {
        let html = r##"# Hello, world!
This is a test markdown document.

```rust
fn main() {
    println!("Hello, world!");
}
```

- Item 1
- Item 2
- Item 3

$\sum_{1}^{2}$
$$E = mc^2$$

![Image](https://www.rust-lang.org/logos/rust-logo-512x512.png)
"##;
        let png_data = md_to_img(html).await.unwrap();
        std::fs::write("screenshot.png", png_data).unwrap();
    }
}
