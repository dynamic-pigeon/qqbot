use std::time::Duration;

use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use kovi::tokio::{self, sync::OnceCell};
use tracing::{error, info};

use crate::{BoundedPool, ResourceManager};

/// 截图默认超时：避免浏览器偶发卡死永久占用一个 tokio task。
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

const PAGE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
const SCREENSHOT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_SCREENSHOTS: usize = 2;
const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCREENSHOT_PIXELS: f64 = 16_000_000.0;

/// 浏览器空闲超过此时间后自动关闭，释放内存和 CPU。
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 对外入口：将 HTML 渲染为 PNG。
/// - `html`: 完整 HTML 内容
/// - `selector`: 若提供，只截取该 CSS 选择器匹配的元素；否则截取全页
pub async fn screenshot(html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
    get_manager().await.screenshot(html, selector).await
}

async fn get_manager() -> &'static ScreenshotManager {
    static MANAGER: OnceCell<ScreenshotManager> = OnceCell::const_new();
    MANAGER
        .get_or_init(|| async { ScreenshotManager::new() })
        .await
}

pub struct ScreenshotManager {
    browser: ResourceManager<Browser>,
    pool: BoundedPool,
}

impl ScreenshotManager {
    fn new() -> Self {
        Self {
            browser: ResourceManager::new_with_destructor(
                IDLE_TIMEOUT,
                || async {
                    info!("launching chromiumoxide browser (lazy init)");
                    Self::launch_browser().await
                },
                Self::close_browser,
            ),
            pool: BoundedPool::new(MAX_CONCURRENT_SCREENSHOTS),
        }
    }

    async fn launch_browser() -> Result<Browser> {
        let config = BrowserConfig::builder()
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
            .map_err(anyhow::Error::msg)?;
        let (browser, mut handler) =
            tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, Browser::launch(config))
                .await
                .map_err(|_| anyhow::anyhow!("启动 Chromium 超时"))??;

        // chromiumoxide 要求持续轮询 handler stream，否则 CDP 事件不会被处理。
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        Ok(browser)
    }

    async fn close_browser(mut browser: Browser) {
        info!("shutting down chromiumoxide browser");
        let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.close()).await;
        let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.wait()).await;
    }

    pub async fn screenshot(&self, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
        if html.len() > MAX_HTML_BYTES {
            anyhow::bail!("HTML 超过截图输入上限: {} bytes", MAX_HTML_BYTES);
        }
        let _permit = self.pool.acquire(SCREENSHOT_WAIT_TIMEOUT).await?;
        let browser = self.browser.get().await?;
        match Self::do_screenshot(&browser, html, selector).await {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                error!("截图失败（{}），尝试重启浏览器后重试", e);
                self.browser.invalidate(&browser);
                drop(browser);
                let browser = self.browser.get().await?;
                Self::do_screenshot(&browser, html, selector).await
            }
        }
    }

    async fn do_screenshot(
        browser: &Browser,
        html: &str,
        selector: Option<&str>,
    ) -> Result<Vec<u8>> {
        let page = tokio::time::timeout(SCREENSHOT_TIMEOUT, browser.new_page("about:blank"))
            .await
            .map_err(|_| anyhow::anyhow!("创建截图页面超时"))??;

        let capture = async {
            page.set_content(html).await?;

            let bytes = if let Some(selector) = selector {
                let bounding_box = page
                    .find_element(selector)
                    .await
                    .map_err(|e| anyhow::anyhow!("找不到元素 {}: {}", selector, e))?
                    .bounding_box()
                    .await?;
                validate_dimensions(bounding_box.width, bounding_box.height)?;
                let viewport = Viewport {
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
                let dimensions: Vec<f64> = page
                    .evaluate_expression(
                        "[Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), Math.max(document.documentElement.scrollHeight, document.body.scrollHeight)]",
                    )
                    .await?
                    .into_value()?;
                if dimensions.len() != 2 {
                    anyhow::bail!("无法获取页面尺寸");
                }
                validate_dimensions(dimensions[0], dimensions[1])?;
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(true)
                        .build(),
                )
                .await?
            };
            Ok(bytes)
        };

        let res = tokio::time::timeout(SCREENSHOT_TIMEOUT, capture)
            .await
            .map_err(|_| anyhow::anyhow!("截图超时"))?;

        let _ = tokio::time::timeout(PAGE_CLOSE_TIMEOUT, page.close()).await;
        res
    }
}

fn validate_dimensions(width: f64, height: f64) -> Result<()> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        anyhow::bail!("无效的截图尺寸: {width}x{height}");
    }
    if width * height > MAX_SCREENSHOT_PIXELS {
        anyhow::bail!("截图像素面积超过上限: {width}x{height}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_dimensions_have_a_hard_limit() {
        assert!(validate_dimensions(1920.0, 1080.0).is_ok());
        assert!(validate_dimensions(10_000.0, 10_000.0).is_err());
        assert!(validate_dimensions(f64::NAN, 100.0).is_err());
        assert!(validate_dimensions(0.0, 100.0).is_err());
    }
}
