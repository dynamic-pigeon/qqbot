use std::{borrow::Cow, sync::Arc, time::Duration};

use anyhow::Result;
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    cdp::browser_protocol::page::CaptureScreenshotFormat,
    page::ScreenshotParams,
};
use futures::StreamExt;
use kovi::{
    tokio::{
        self,
        sync::{OnceCell, mpsc, oneshot},
        task::JoinSet,
        time::{self, Instant},
    },
};
use tracing::{error, info};

/// 浏览器空闲超时时间，超过此时间没有截图任务则自动关闭浏览器
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 截图任务：包含 HTML 内容、选择器和用于返回结果的 oneshot 通道
struct ScreenshotTask {
    html: Cow<'static, str>,
    selector: Option<Cow<'static, str>>,
    resp: oneshot::Sender<Result<Vec<u8>>>,
}

/// 截图 HTML 内容，返回 PNG 数据
/// - `html`: 要截图的 HTML 内容
/// - `selector`: 可选的 CSS 选择器，如果提供则只截图该元素，否则截图整个页面
///
/// 浏览器会在首次调用时启动，空闲超过 5 分钟后自动关闭，下次调用时重新启动。
pub async fn screenshot(
    html: Cow<'static, str>,
    selector: Option<Cow<'static, str>>,
) -> Result<Vec<u8>> {
    let (resp_tx, resp_rx) = oneshot::channel();
    send_task(ScreenshotTask {
        html,
        selector,
        resp: resp_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("Screenshot worker channel closed"))?;
    resp_rx
        .await
        .map_err(|_| anyhow::anyhow!("Screenshot worker dropped response"))?
}

async fn send_task(task: ScreenshotTask) -> Result<()> {
    let tx = get_task_sender().await;
    tx.send(task)
        .await
        .map_err(|_| anyhow::anyhow!("Screenshot worker channel closed"))
}

#[inline]
async fn get_task_sender() -> &'static mpsc::Sender<ScreenshotTask> {
    static TX: OnceCell<mpsc::Sender<ScreenshotTask>> = OnceCell::const_new();
    TX.get_or_init(|| async {
        let (tx, rx) = mpsc::channel(32);
        kovi::spawn(screenshot_worker(rx));
        tx
    })
    .await
}

/// 后台 worker：并发执行截图任务，空闲超时则关闭浏览器释放资源
async fn screenshot_worker(mut rx: mpsc::Receiver<ScreenshotTask>) {
    let mut browser: Option<Arc<Browser>> = None;
    let mut tasks: JoinSet<()> = JoinSet::new();

    let idle_deadline = time::sleep(IDLE_TIMEOUT);
    let mut idle_deadline = std::pin::pin!(idle_deadline);

    loop {
        let idle_eligible = browser.is_some() && tasks.is_empty();

        tokio::select! {
            biased;

            // 优先处理已完成的并发任务
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = result {
                    error!("Screenshot task panicked: {}", e);
                }
                // 所有任务完成后，重置空闲计时
                if tasks.is_empty() && browser.is_some() {
                    idle_deadline.as_mut().reset(Instant::now() + IDLE_TIMEOUT);
                }
            }

            // 接收新截图任务，并发派发
            Some(task) = rx.recv() => {
                // 有新任务，重置空闲计时
                idle_deadline.as_mut().reset(Instant::now() + IDLE_TIMEOUT);

                // 确保浏览器已启动
                if browser.is_none() {
                    info!("Screenshot worker: launching browser");
                    match launch_browser().await {
                        Ok(b) => browser = Some(Arc::new(b)),
                        Err(e) => {
                            let _ = task.resp.send(Err(e));
                            continue;
                        }
                    }
                }

                let b = Arc::clone(browser.as_ref().unwrap());
                tasks.spawn(async move {
                    let result = do_screenshot(&b, (*task.html).as_ref(), task.selector.as_deref()).await;
                    let _ = task.resp.send(result);
                });
            }

            // 空闲超时：浏览器已启动且无活跃任务时生效
            () = &mut idle_deadline, if idle_eligible => {
                info!("Screenshot worker: idle timeout, shutting down browser");
                if let Some(b) = browser.take()
                    && let Ok(mut b) = Arc::try_unwrap(b)
                {
                        let _ = b.close().await;
                        let _ = b.wait().await;
                }
            }

            // 通道关闭且无活跃任务，退出 worker
            else => break,
        }
    }

    // 等待剩余任务完成
    while tasks.join_next().await.is_some() {}

    // 退出前关闭浏览器
    if let Some(b) = browser.take()
        && let Ok(mut b) = Arc::try_unwrap(b)
    {
        let _ = b.close().await;
        let _ = b.wait().await;
    }
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
    kovi::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    Ok(browser)
}

async fn do_screenshot(browser: &Browser, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
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
