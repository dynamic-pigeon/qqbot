use std::time::Duration;

use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use kovi::tokio::{self, sync::OnceCell};
use tracing::{error, info};

/// 截图默认超时：避免浏览器偶发卡死永久占用一个 tokio task。
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// 对外入口：将 HTML 渲染为 PNG。
/// - `html`: 完整 HTML 内容
/// - `selector`: 若提供，只截取该 CSS 选择器匹配的元素；否则截取全页
pub async fn screenshot(html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
    let manager = get_manager().await?;
    manager.screenshot(html, selector).await
}

async fn get_manager() -> Result<&'static ScreenshotManager> {
    static MANAGER: OnceCell<ScreenshotManager> = OnceCell::const_new();
    MANAGER.get_or_try_init(ScreenshotManager::init).await
}

pub struct ScreenshotManager {
    browser: tokio::sync::RwLock<Browser>,
    restart_lock: tokio::sync::Mutex<()>,
}

impl ScreenshotManager {
    pub async fn init() -> Result<Self> {
        let browser = Self::launch_browser().await?;
        info!("chromiumoxide browser launched");
        Ok(Self {
            browser: tokio::sync::RwLock::new(browser),
            restart_lock: tokio::sync::Mutex::new(()),
        })
    }

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

    pub async fn screenshot(&self, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
        let browser = self.browser.read().await;
        match tokio::time::timeout(
            SCREENSHOT_TIMEOUT,
            Self::do_screenshot(&browser, html, selector),
        )
        .await
        {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => {
                error!("截图失败，尝试重启浏览器后重试: {}", e);
                drop(browser);
                self.restart_browser().await?;
                let browser = self.browser.read().await;
                tokio::time::timeout(
                    SCREENSHOT_TIMEOUT,
                    Self::do_screenshot(&browser, html, selector),
                )
                .await
                .map_err(|_| anyhow::anyhow!("截图超时"))?
            }
            Err(_) => {
                drop(browser);
                Err(anyhow::anyhow!("截图超时"))
            }
        }
    }

    async fn restart_browser(&self) -> Result<()> {
        let _lock = self.restart_lock.lock().await;
        let mut browser = self.browser.write().await;
        info!("restarting chromiumoxide browser");
        let _ = browser.close().await;
        let _ = browser.wait().await;
        *browser = Self::launch_browser().await?;
        Ok(())
    }

    async fn do_screenshot(
        browser: &Browser,
        html: &str,
        selector: Option<&str>,
    ) -> Result<Vec<u8>> {
        let page = browser.new_page("about:blank").await?;

        let res = async {
            page.set_content(html).await?;

            let bytes = if let Some(selector) = selector {
                let bounding_box = page
                    .find_element(selector)
                    .await
                    .map_err(|e| anyhow::anyhow!("找不到元素 {}: {}", selector, e))?
                    .bounding_box()
                    .await?;
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
}
