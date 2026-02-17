use std::time::Duration;

use anyhow::Result;
use chromiumoxide::{
    browser::{self, Browser, BrowserConfig},
    cdp::browser_protocol::page::CaptureScreenshotFormat,
    page::ScreenshotParams,
};
use futures::StreamExt;
use kovi::{
    log::error,
    tokio::{self, sync::OnceCell},
};

pub async fn screenshot<T: AsRef<str>>(html: T, selector: Option<&str>) -> Result<Vec<u8>> {
    let manager = get_screenshot_manager().await?;
    manager.screenshot(html, selector).await
}

async fn get_screenshot_manager() -> Result<&'static ScreenshotManager> {
    static SCREEN_SHOT: OnceCell<ScreenshotManager> = OnceCell::const_new();
    SCREEN_SHOT
        .get_or_try_init(async || ScreenshotManager::init().await)
        .await
}

pub struct ScreenshotManager {
    browser: tokio::sync::RwLock<Browser>,
    lock: tokio::sync::Mutex<()>,
}

impl ScreenshotManager {
    pub async fn init() -> Result<Self> {
        let browser = Self::launch_browser().await?;
        Ok(Self {
            browser: tokio::sync::RwLock::new(browser),
            lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn launch_browser() -> Result<Browser> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .no_sandbox()
                .request_timeout(Duration::from_secs(1))
                .window_size(1920, 1080)
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

    pub async fn screenshot<T: AsRef<str>>(
        &self,
        html: T,
        selector: Option<&str>,
    ) -> Result<Vec<u8>> {
        let html_ref = html.as_ref();

        // 首次尝试截图
        let browser = self.browser.read().await;
        match Self::do_screenshot(&browser, html_ref, selector).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => error!("Screenshot error: {}", e),
        }

        // 如果失败，重启浏览器后重试
        error!("Screenshot failed, restarting browser...");
        drop(browser); // 释放读锁
        let mut browser = self.browser.write().await; // 获取写锁重启浏览器
        if let Ok(_lock) = self.lock.try_lock() {
            browser.close().await?;
            browser.wait().await?;
            *browser = Self::launch_browser().await?;
            // 保证写锁在 lock 之前释放
            drop(browser);
        } else {
            // 如果无法获取锁，说明其他线程正在重启浏览器
            drop(browser);
        }

        // 重试截图
        let browser = self.browser.read().await;
        Self::do_screenshot(&browser, html_ref, selector).await
    }

    async fn do_screenshot(
        browser: &Browser,
        html: &str,
        selector: Option<&str>,
    ) -> Result<Vec<u8>> {
        let page = browser.new_page("about:blank").await?;
        // 总之这样能保证 page.close() 能执行到
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
        page.close().await?;

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_screenshot() {
        let manager = ScreenshotManager::init().await.unwrap();
        let html = r#"
            <html>
                <body>
                    <div id="test" style="width: 200px; height: 100px; background: red;"></div>
                </body>
            </html>
        "#;
        let png_data = manager.screenshot(html, Some("#test")).await.unwrap();
        std::fs::write("test_screenshot.png", png_data).unwrap();
    }
}
