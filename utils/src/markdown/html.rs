pub static HTML_START_NEXT_IS_MD_CSS: &str = concat!(
    r#"<!doctype html>
<html>
<head>
<meta charset="UTF-8">
<!-- KaTeX 核心样式 -->
<style>
"#,
    include_str!("html/katex.min.css"),
    r#"
</style>
<!-- 自定义数学公式样式 -->
<style>
    .math { margin: 1em 0; }
    .katex { font-size: 1.1em; }
</style>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>"#
);

pub static HTML_2_NEXT_IS_HIGHLIGHT_CSS: &str = r#"
.markdown-body {
    box-sizing: border-box;
    width: 100%;
    max-width: 720px;
    margin: 0;
    padding: 12px 12px 20px 12px;
    height: auto;

    font-family: "MiSans", -apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif, "Apple Color Emoji", "Segoe UI Emoji";
}

body{
    font-family: Arial, sans-serif; /* 选择无衬线字体 */
    margin: 0;
    padding: 0;
    overflow: hidden;
}
</style>
<style>
"#;

pub static HTML_3_NEXT_IS_MD_BODY_AND_THEN_IS_HTML4: &str = r#"</style>
</head>
<body>
<article class="markdown-body">"#;

pub static HTML_4_NEXT_IS_HIGH_LIGHT_JS: &str = "</article><script>";

pub static HTML_SCRIPT: &str = include_str!("html/katex.min.js");

pub static HTML_END: &str = r#"</script><script>hljs.highlightAll();</script>
<script>
const elementsToCheck = ['pre', 'code']; // 需要检测的元素

document.addEventListener("DOMContentLoaded", function() {
    const markdownBody = document.querySelector('.markdown-body');
    let foundElement = false;

    elementsToCheck.forEach(tag => {
        if (markdownBody.querySelector(tag)) {
            foundElement = true;
        }
    });

    if (foundElement) {
        markdownBody.style.maxWidth = '720px';
    } else {
        markdownBody.style.maxWidth = '500px';
    }

    const finishedElement = document.createElement('div');

    finishedElement.classList.add('finish');

    // 完成页面加载
    document.body.appendChild(finishedElement);
});
</script>
<script>
    document.addEventListener('DOMContentLoaded', () => {
    // 渲染行内公式
    document.querySelectorAll('.math-inline').forEach(el => {
        const tex = el.textContent;
        const span = document.createElement('span');
        katex.render(tex, span, { displayMode: false });
        el.replaceWith(span);
    });
    // 渲染块级公式
    document.querySelectorAll('.math-display').forEach(el => {
        const tex = el.textContent;
        const div = document.createElement('div');
        katex.render(tex, div, { displayMode: true });
        el.replaceWith(div);
    });
    });
</script>
"#;

pub static END: &str = r#"</body></html>"#;

pub static HIGH_LIGHT_JS_NEXT_IS_HTML_END: &str = include_str!("html/highlight.js");

// pub static HIGH_LIGHT_DARK_CSS_NEXT_IS_HTML3: &str = include_str!("html/highlight_github_dark.css");

pub static HIGH_LIGHT_LIGHT_CSS_NEXT_IS_HTML3: &str =
    include_str!("html/highlight_github_light.css");

pub static GITHUB_MARKDOWN_LIGHT_NEXT_IS_HTML2: &str = include_str!("html/github_md_light.css");

// pub static GITHUB_MARKDOWN_DARK_NEXT_IS_HTML2: &str = include_str!("html/github_md_dark.css");
