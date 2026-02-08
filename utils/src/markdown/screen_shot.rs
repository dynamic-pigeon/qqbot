use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use kovi::log::error;
use kovi::tokio;
use std::sync::Arc;
use std::time::Duration;

pub struct ScreenshotManager {
    browser: Arc<tokio::sync::Mutex<Browser>>,
}

impl ScreenshotManager {
    pub async fn init() -> Result<Self> {
        let browser = Self::launch_browser().await?;
        Ok(Self {
            browser: Arc::new(tokio::sync::Mutex::new(browser)),
        })
    }

    async fn launch_browser() -> Result<Browser> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .no_sandbox()
                .request_timeout(Duration::from_secs(1))
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await?;
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        Ok(browser)
    }

    pub async fn screenshot<T: AsRef<[u8]>>(&self, html: T) -> Result<Vec<u8>> {
        let html_ref = html.as_ref();

        // 首次尝试截图
        let mut browser = self.browser.lock().await;
        if let Ok(bytes) = Self::do_screenshot(&browser, html_ref)
            .await
            .inspect_err(|e| error!("Screenshot error: {}", e))
        {
            return Ok(bytes);
        }

        // 如果失败，重启浏览器后重试
        error!("Screenshot failed, restarting browser...");
        let _ = browser.close().await;
        match Self::launch_browser().await {
            Ok(new_browser) => {
                *browser = new_browser;
                Self::do_screenshot(&browser, html_ref).await
            }
            Err(e) => Err(anyhow::anyhow!("Failed to restart browser: {}", e)),
        }
    }

    async fn do_screenshot(browser: &Browser, html: &[u8]) -> Result<Vec<u8>> {
        let page = browser.new_page("about:blank").await?;
        // 总之这样能保证 page.close() 能执行到
        let res = async {
            page.set_content(std::str::from_utf8(html)?).await?;

            let bounding_box = page
                .find_element("article.markdown-body")
                .await?
                .bounding_box()
                .await?;
            let viewport = chromiumoxide::cdp::browser_protocol::page::Viewport {
                x: bounding_box.x,
                y: bounding_box.y,
                width: bounding_box.width,
                height: bounding_box.height,
                scale: 1.0,
            };
            let bytes = page
                .screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .clip(viewport)
                        .capture_beyond_viewport(true)
                        .build(),
                )
                .await?;
            Ok(bytes)
        }
        .await;
        page.close().await?;

        res
    }
}
