use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use kovi::tokio;
use std::sync::Arc;

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
        {
            let browser = self.browser.lock().await;
            if let Ok(bytes) = self.do_screenshot(&browser, html_ref).await {
                return Ok(bytes);
            }
        }

        // 如果失败，重启浏览器后重试
        eprintln!("Screenshot failed, restarting browser...");
        {
            let mut browser_lock = self.browser.lock().await;
            match Self::launch_browser().await {
                Ok(new_browser) => {
                    *browser_lock = new_browser;
                    self.do_screenshot(&browser_lock, html_ref).await
                }
                Err(e) => Err(anyhow::anyhow!("Failed to restart browser: {}", e)),
            }
        }
    }

    async fn do_screenshot(&self, browser: &Browser, html: &[u8]) -> Result<Vec<u8>> {
        let page = browser.new_page("about:blank").await?;
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
                    .full_page(true)
                    .build(),
            )
            .await?;
        page.close().await?;

        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_screenshot() {
        let manager = ScreenshotManager::init().await.unwrap();
        let html = "<html><body><h1>Hello, world!</h1></body></html>";
        let png_data = manager.screenshot(html).await.unwrap();
        std::fs::write("screenshot.png", png_data).unwrap();
    }
}
