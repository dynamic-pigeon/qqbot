use std::time::Duration;

use anyhow::Result;
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::page::ScreenshotParams;
use kovi::tokio::{self, sync::OnceCell};
use tracing::{debug, error};

use crate::{BoundedPool, ChromiumInstance, ChromiumLaunch, ResourceManager};

/// 截图默认超时：避免浏览器偶发卡死永久占用一个 tokio task。
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

const PAGE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
const SCREENSHOT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
/// 等待选择器出现时的轮询间隔。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_CONCURRENT_SCREENSHOTS: usize = 2;
const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCREENSHOT_PIXELS: f64 = 16_000_000.0;
/// 按 2 倍设备像素截图，手机和 Retina 上看文字更清楚。
const SCREENSHOT_SCALE: f64 = 2.0;

/// 浏览器空闲超过此时间后自动关闭，释放内存和 CPU。
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 对外入口：将 HTML 渲染为 PNG，行为由 [`ScreenshotOptions`] 控制。
pub async fn screenshot(html: &str, options: ScreenshotOptions<'_>) -> Result<Vec<u8>> {
    get_manager().await.screenshot(html, options).await
}

/// 一次截图的配置。
///
/// 用 builder 链式配置，`Default` 表示“截全页、不等待”。
#[derive(Default, Copy, Clone)]
pub struct ScreenshotOptions<'a> {
    /// 若提供，只截取该 CSS 选择器匹配的元素；否则截取全页。
    selector: Option<&'a str>,
    /// 截图前需要等待出现的 CSS 选择器，每个最多等 `SCREENSHOT_WAIT_TIMEOUT`，
    /// 任一超时则整体失败；空切片表示不等待。
    wait_selectors: &'a [&'a str],
}

impl<'a> ScreenshotOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_selector(mut self, selector: &'a str) -> Self {
        self.selector = Some(selector);
        self
    }

    pub fn with_wait_selectors(mut self, wait_selectors: &'a [&'a str]) -> Self {
        self.wait_selectors = wait_selectors;
        self
    }
}

async fn get_manager() -> &'static ScreenshotManager {
    static MANAGER: OnceCell<ScreenshotManager> = OnceCell::const_new();
    MANAGER
        .get_or_init(|| async { ScreenshotManager::new() })
        .await
}

pub(crate) struct ScreenshotManager {
    browser: ResourceManager<ChromiumInstance>,
    pool: BoundedPool,
}

impl ScreenshotManager {
    fn new() -> Self {
        Self {
            browser: ChromiumLaunch::new("screenshot")
                .flags([
                    "disable-dev-shm-usage",
                    "disable-background-networking",
                    "disable-default-apps",
                    "disable-extensions",
                    "disable-sync",
                    "disable-translate",
                    "no-first-run",
                    "mute-audio",
                    "use-mock-keychain",
                ])
                .lifecycle_timeout(BROWSER_LIFECYCLE_TIMEOUT)
                .managed(IDLE_TIMEOUT),
            pool: BoundedPool::new(MAX_CONCURRENT_SCREENSHOTS),
        }
    }

    pub async fn screenshot(&self, html: &str, options: ScreenshotOptions<'_>) -> Result<Vec<u8>> {
        if html.len() > MAX_HTML_BYTES {
            anyhow::bail!("HTML 超过截图输入上限: {} bytes", MAX_HTML_BYTES);
        }
        let _permit = self.pool.acquire(SCREENSHOT_WAIT_TIMEOUT).await?;
        let browser = self.browser.get().await?;
        match Self::do_screenshot(&browser, html, options).await {
            Ok(bytes) => Ok(bytes),
            // 等待元素超时说明页面没达到预期状态，重启浏览器是白费等第二次，
            // 直接返回；其余错误按“浏览器卡死”处理，重启后重试一次。
            Err(e) if e.downcast_ref::<ElementWaitTimeoutError>().is_some() => Err(e),
            Err(e) => {
                error!("截图失败（{}），尝试重启浏览器后重试", e);
                // replace 会等旧实例关闭后再启动；新实例用独立 profile，不抢旧进程的锁。
                let browser = self.browser.replace(browser).await?;
                Self::do_screenshot(&browser, html, options).await
            }
        }
    }

    async fn do_screenshot(
        browser: &Browser,
        html: &str,
        options: ScreenshotOptions<'_>,
    ) -> Result<Vec<u8>> {
        let page = tokio::time::timeout(SCREENSHOT_TIMEOUT, browser.new_page("about:blank"))
            .await
            .map_err(|_| anyhow::anyhow!("创建截图页面超时"))??;

        let capture = async {
            page.set_content(html).await?;

            for wait_selector in options.wait_selectors {
                wait_for_selector(&page, wait_selector).await?;
            }

            let bytes = if let Some(selector) = options.selector {
                let bounding_box = page
                    .find_element(selector)
                    .await
                    .map_err(|e| anyhow::anyhow!("找不到元素 {}: {}", selector, e))?
                    .bounding_box()
                    .await?;
                validate_dimensions(bounding_box.width, bounding_box.height, SCREENSHOT_SCALE)?;
                let viewport = Viewport {
                    x: bounding_box.x,
                    y: bounding_box.y,
                    width: bounding_box.width,
                    height: bounding_box.height,
                    scale: SCREENSHOT_SCALE,
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
                validate_dimensions(dimensions[0], dimensions[1], SCREENSHOT_SCALE)?;
                let viewport = Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: dimensions[0],
                    height: dimensions[1],
                    scale: SCREENSHOT_SCALE,
                };
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .clip(viewport)
                        .capture_beyond_viewport(true)
                        .build(),
                )
                .await?
            };
            Ok(bytes)
        };

        let res = tokio::time::timeout(SCREENSHOT_TIMEOUT, capture)
            .await
            .map_err(|_| anyhow::anyhow!("截图超时"))?;

        // close 超时说明页面或浏览器卡住，页面会残留在浏览器进程内；
        // 由 5 分钟 idle 回收或下次错误触发的浏览器整体重启兜底。
        if tokio::time::timeout(PAGE_CLOSE_TIMEOUT, page.close())
            .await
            .is_err()
        {
            debug!("页面关闭超时，页面残留在浏览器进程内，等待浏览器整体回收");
        }
        res
    }
}

/// 元素等待超时的标记错误：页面状态未达预期，不代表浏览器异常，
/// 重启浏览器不会改变结果，上层据此跳过“重启后重试”。
#[derive(Debug, thiserror::Error)]
#[error("等待元素 `{selector}` 出现超时")]
struct ElementWaitTimeoutError {
    selector: String,
}

/// 轮询等待 CSS 选择器匹配的元素出现，最多等 `SCREENSHOT_WAIT_TIMEOUT`。
/// chromiumoxide 的 `find_element` 是一次性查询、不做等待，所以这里显式轮询。
async fn wait_for_selector(page: &chromiumoxide::Page, selector: &str) -> Result<()> {
    tokio::time::timeout(SCREENSHOT_WAIT_TIMEOUT, async {
        loop {
            if page.find_element(selector).await.is_ok() {
                return;
            }
            tokio::time::sleep(WAIT_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(ElementWaitTimeoutError {
            selector: selector.to_string(),
        })
    })
}

fn validate_dimensions(width: f64, height: f64, scale: f64) -> Result<()> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        anyhow::bail!("无效的截图尺寸: {width}x{height}");
    }
    if !scale.is_finite() || scale <= 0.0 {
        anyhow::bail!("无效的截图缩放: {scale}");
    }
    if width * scale * height * scale > MAX_SCREENSHOT_PIXELS {
        anyhow::bail!("截图像素面积超过上限: {width}x{height}@{scale}x");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_dimensions_have_a_hard_limit() {
        assert!(validate_dimensions(1920.0, 1080.0, SCREENSHOT_SCALE).is_ok());
        assert!(validate_dimensions(10_000.0, 10_000.0, 1.0).is_err());
        assert!(validate_dimensions(f64::NAN, 100.0, SCREENSHOT_SCALE).is_err());
        assert!(validate_dimensions(0.0, 100.0, SCREENSHOT_SCALE).is_err());
        assert!(validate_dimensions(100.0, 100.0, 0.0).is_err());
    }
}
