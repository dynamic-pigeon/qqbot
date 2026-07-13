use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use kovi::tokio::{self, sync::OnceCell};
use tracing::{error, info};

use crate::BoundedPool;

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
    let manager = get_manager().await?;
    manager.screenshot(html, selector).await
}

async fn get_manager() -> Result<&'static ScreenshotManager> {
    static MANAGER: OnceCell<ScreenshotManager> = OnceCell::const_new();
    MANAGER
        .get_or_try_init(|| async { Ok(ScreenshotManager::new()) })
        .await
}

pub struct ScreenshotManager {
    /// 浏览器实例：`None` 表示未启动或已被空闲回收。
    browser: tokio::sync::RwLock<Option<Browser>>,
    /// 串行化浏览器启动 / 关闭 / 重启，避免竞态。
    lifecycle_lock: tokio::sync::Mutex<()>,
    /// 每次截图递增的代数，空闲回收任务通过比对代数判断浏览器是否仍在使用。
    generation: AtomicU64,
    pool: BoundedPool,
    idle_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ScreenshotManager {
    fn new() -> Self {
        Self {
            browser: tokio::sync::RwLock::new(None),
            lifecycle_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            pool: BoundedPool::new(MAX_CONCURRENT_SCREENSHOTS),
            idle_task: Mutex::new(None),
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

    /// 确保浏览器在运行：已存在则直接返回，不存在则启动一个。
    async fn ensure_browser(&self) -> Result<()> {
        if self.browser.read().await.is_some() {
            return Ok(());
        }

        let _lock = self.lifecycle_lock.lock().await;
        // 双重检查：其他任务可能在我们等锁期间启动了浏览器
        if self.browser.read().await.is_some() {
            return Ok(());
        }

        info!("launching chromiumoxide browser (lazy init)");
        let browser = Self::launch_browser().await?;
        *self.browser.write().await = Some(browser);
        Ok(())
    }

    /// 关闭当前浏览器并启动新的。调用方应已确认浏览器存在。
    async fn restart_browser(&self) -> Result<()> {
        let _lock = self.lifecycle_lock.lock().await;
        let mut guard = self.browser.write().await;
        if let Some(mut browser) = guard.take() {
            info!("restarting chromiumoxide browser");
            let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.close()).await;
            let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.wait()).await;
        }
        *guard = Some(Self::launch_browser().await?);
        Ok(())
    }

    /// 空闲回收：等待 [`IDLE_TIMEOUT`] 后，若代数未变则关闭浏览器。
    ///
    /// 由 [`screenshot`] 安排单个可取消的后台任务调用。
    async fn try_close_idle(&self, snapshot: u64) {
        tokio::time::sleep(IDLE_TIMEOUT).await;

        // 期间有新的截图请求 → 不回收
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }

        let _lock = self.lifecycle_lock.lock().await;
        // 获取锁后再次检查，避免与正在排队的 ensure_browser 竞态
        if self.generation.load(Ordering::Relaxed) != snapshot {
            return;
        }

        if let Some(mut browser) = self.browser.write().await.take() {
            info!(
                "chromiumoxide browser idle for {:?}, shutting down",
                IDLE_TIMEOUT
            );
            let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.close()).await;
            let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.wait()).await;
        }
    }

    pub async fn screenshot(&self, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
        if html.len() > MAX_HTML_BYTES {
            anyhow::bail!("HTML 超过截图输入上限: {} bytes", MAX_HTML_BYTES);
        }
        let _permit = self.pool.acquire(SCREENSHOT_WAIT_TIMEOUT).await?;
        self.ensure_browser().await?;

        // 递增代数并记录当前值，供空闲回收任务比对
        let snapshot = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);

        let result = {
            let guard = self.browser.read().await;
            let browser = guard
                .as_ref()
                .expect("browser must exist after ensure_browser");

            match Self::do_screenshot(browser, html, selector).await {
                Ok(bytes) => Ok(bytes),
                Err(e) => {
                    error!("截图失败，尝试重启浏览器后重试: {}", e);
                    drop(guard);
                    self.restart_browser().await?;
                    let guard = self.browser.read().await;
                    let browser = guard.as_ref().expect("browser must exist after restart");
                    Self::do_screenshot(browser, html, selector).await
                }
            }
        };

        self.schedule_idle_cleanup(snapshot);

        result
    }

    fn schedule_idle_cleanup(&self, snapshot: u64) {
        let mut task = self
            .idle_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = task.take() {
            previous.abort();
        }
        *task = Some(tokio::spawn(try_close_idle(snapshot)));
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

/// 后台空闲回收入口：从静态 [`ScreenshotManager`] 读取当前代数并尝试关闭。
async fn try_close_idle(snapshot: u64) {
    // Manager 在首次截图时已初始化，这里只会取到已存在的实例。
    if let Ok(manager) = get_manager().await {
        manager.try_close_idle(snapshot).await;
    }
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
