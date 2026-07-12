# chromiumoxide 迁移设计文档

## 背景与目标

当前 `plugins/utils` 使用 `playwright-rs` 启动 headless Chromium 完成 HTML → PNG 截图。该依赖的 `build.rs` 会在构建期从 Azure CDN 下载未签名的 Node.js 驱动并解压执行，存在供应链风险。

本设计目标：将截图实现从 `playwright-rs` 迁移回 `chromiumoxide`，尽量复用 git 历史中已有的实现（commit `435671a` 及之前），并吸收当前 `playwright` 代码中的安全加固与超时控制。

## 当前状态

- 截图入口：`plugins/utils/src/screen_shot.rs`
- 当前依赖：`playwright-rs.workspace = true`
- 当前对外接口：
  - `utils::screenshot(html, selector)`（被 `markdown`、`msg_rank` 插件使用）
  - `utils::get_context()`、`utils::ContextGuard`（已导出但无调用方）
- 当前设计：全局 `BrowserSession` + `BoundedResourcePool<BrowserContext>`，复用 BrowserContext

## 迁移方案：最小恢复

### 总体思路

恢复 git 历史中基于 `chromiumoxide` 的 `ScreenshotManager` 结构，对外只保留 `screenshot(html, selector)`。每次截图新建一个 `Page`，截图完成后关闭；不再维护 `BrowserContext` 资源池。

### 依赖变更

`plugins/utils/Cargo.toml`：

```toml
[dependencies]
chromiumoxide = "0.9"  # 当前 crates.io 最新稳定版
futures = "0.3"
# 移除 playwright-rs
```

`Cargo.toml` workspace：移除 `playwright-rs` workspace dependency（确认无其他 crate 使用）。

### 浏览器启动

```rust
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

async fn launch_browser() -> Result<Browser> {
    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .window_size(1920, 1080)
            .arg("--disable-dev-shm-usage")
            .arg("--disable-background-networking")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--no-first-run")
            .arg("--mute-audio")
            .arg("--password-store=basic")
            .arg("--use-mock-keychain")
            .build()
            .map_err(anyhow::Error::msg)?,
    )
    .await?;

    // chromiumoxide 要求持续轮询 handler stream，否则 CDP 消息不会被处理
    kovi::tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    Ok(browser)
}
```

注意：
- 不再使用旧代码中的 `.no_sandbox()`，保持沙箱开启。
- 不再使用旧代码中极短的 `request_timeout(Duration::from_secs(1))`。
- 启动参数沿用当前 playwright 版本的加固参数。

### 截图流程

```rust
async fn do_screenshot(browser: &Browser, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
    let page = browser.new_page("about:blank").await?;

    let res = async {
        page.set_content(html).await?;

        let bytes = if let Some(selector) = selector {
            let bounding_box = page.find_element(selector).await?.bounding_box().await?;
            let viewport = chromiumoxide::cdp::browser_protocol::page::Viewport {
                x: bounding_box.x,
                y: bounding_box.y,
                width: bounding_box.width,
                height: bounding_box.height,
                scale: 1.0,
            };
            page.screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .clip(viewport)
                    .capture_beyond_viewport(true)
                    .build(),
            )
            .await?
        } else {
            page.screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(true)
                    .build(),
            )
            .await?
        };
        Ok(bytes)
    }
    .await;

    let _ = page.close().await;
    res
}
```

### 错误重试

保留当前的重试语义：首次截图失败时，关闭并重启浏览器后重试一次。

```rust
pub async fn screenshot(html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
    let manager = get_manager().await?;

    match manager.do_screenshot(html, selector).await {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            tracing::error!("截图失败，尝试重启浏览器后重试: {}", e);
            manager.restart_browser().await?;
            manager.do_screenshot(html, selector).await
        }
    }
}
```

### 对外接口

```rust
// plugins/utils/src/lib.rs
pub use screen_shot::screenshot;

// 移除：
// pub use screen_shot::{ContextGuard, get_context};
```

`markdown` 与 `msg_rank` 插件无需改动，因为它们只调用 `utils::screenshot(...)`。

### Chromium 来源

依赖运行环境已安装的 Chrome/Chromium。`chromiumoxide` 会按常见路径自动查找：
- `google-chrome`
- `google-chrome-stable`
- `chromium`
- `chromium-browser`
- macOS `/Applications/Google Chrome.app/...`

部署文档需要说明：在 Dockerfile/系统包管理器中安装 Chromium。

## 不变更的内容

- `plugins/utils/src/bounded_pool.rs` 保留，虽然截图不再使用，但未来可能复用，且移除会破坏 git 历史整洁性（本次不涉及）。
- Markdown HTML 模板、KaTeX/highlight.js 资源、ammonia sanitizer 均保持不变。
- `msg_rank` 的发言排行 HTML 模板保持不变。

## 测试计划

1. `cargo check` 与 `cargo clippy` 通过。
2. 运行 `plugins/utils/tests/markdown.rs` 中现有截图测试（如存在）。
3. 手动触发 `/wordcloud once` 或 `#今日发言排行`，验证图片正常生成。
4. 验证以下场景：
   - 普通 Markdown（标题、列表、代码块）
   - KaTeX 数学公式
   - 代码高亮
   - 中文字体显示
   - 无 selector 的全页截图（msg_rank）
   - 带 selector 的元素截图（markdown）

## 风险与回滚

- **风险**：新版 `chromiumoxide` API 与 git 旧代码（0.8.0）有差异，可能需要调整 import 和 builder 调用。
- **风险**：运行环境未安装 Chromium 时启动失败，错误信息需要清晰。
- **回滚**：保留 `playwright-rs` 版本在 git 历史，必要时 `git revert` 即可恢复。

## 后续可选优化

- 如截图频率变高，可重新引入 Page/Context 池化（方案 2）。
- 如部署环境无法预装 Chromium，可评估 `chromiumoxide_fetcher` 自动下载（方案 3），但需校验下载哈希。
