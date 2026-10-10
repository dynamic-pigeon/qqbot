//! 图库派生特征（指纹/SIFT）的解码管道：读盘走 async、解码占 blocking
//! 线程，按解码后大小分道——小图多 worker 并行，大图单 worker 串行。

use std::path::PathBuf;

use super::StoreError;

/// 解码后的像素缓冲（RGB + luma 副本按每像素 4 字节估算）超过此值的图
/// 不进并行队列，交给单独的串行队列：多张大缓冲同时解会把峰值内存叠到
/// 数倍，在无 swap、余量紧张的部署机上不可接受。文件字节是被压缩过的
/// 差代理（一张 4 MiB 的 JPEG 也能解出 6000×8000），分道看解码后大小。
const PARALLEL_DECODE_LIMIT: u64 = 32 * 1024 * 1024;

/// 头部预读字节数：PNG IHDR 在前 33 字节，JPEG 的 SOF 通常也在头部；
/// 带 Exif 缩略图的 JPEG 段可能更长，解析不出时退回文件字节兜底。
const DIMENSION_HEAD: usize = 64 * 1024;

/// 读文件头估出解码后大小，决定走并行还是串行队列。
/// blob 按内容寻址、写入后不变，头部足以定案。
pub(super) fn is_large_blob(path: &PathBuf) -> bool {
    let mut head = vec![0u8; DIMENSION_HEAD];
    let read =
        std::fs::File::open(path).and_then(|mut file| std::io::Read::read(&mut file, &mut head));
    match read {
        Ok(n) => {
            head.truncate(n);
            match crate::similar::pixel_dimensions(&head) {
                Some((w, h)) => u64::from(w) * u64::from(h) * 4 > PARALLEL_DECODE_LIMIT,
                // 头解析不出：退回文件字节判据，宁可串行也不放进并行。
                None => std::fs::metadata(path).is_ok_and(|meta| meta.len() > 4 * 1024 * 1024),
            }
        }
        Err(_) => true,
    }
}

/// 单个解码 worker：固定数量的 async 任务，从队列动态领活，同一时刻
/// 只挂一个 blocking 解码，所以占用的解码线程数恒等于 worker 数。
/// 队列关闭（发送端全部 drop）且排空后 `recv` 返回 Err，worker 自然退出。
fn spawn_derive_worker<T, F>(
    rx: async_channel::Receiver<(String, Vec<u8>)>,
    compute: F,
) -> kovi::tokio::task::JoinHandle<anyhow::Result<Vec<(String, T)>>>
where
    T: Send + 'static,
    F: Fn(&[u8]) -> Option<T> + Send + Sync + Clone + 'static,
{
    kovi::tokio::spawn(async move {
        let mut computed = Vec::new();
        while let Ok((hash, bytes)) = rx.recv().await {
            let derived = kovi::tokio::task::spawn_blocking({
                let compute = compute.clone();
                move || compute(&bytes)
            })
            .await
            .map_err(|e| anyhow::anyhow!("计算派生特征失败: {e}"))?;
            if let Some(derived) = derived {
                computed.push((hash, derived));
            }
        }
        Ok(computed)
    })
}

/// 读盘走 async，解码占 blocking 线程。队列是 async-channel（MPMC，
/// Receiver 可 Clone）：一条小图队列 workers 个 worker 动态领活、谁快
/// 谁多拿；一条大图队列单 worker，同一时刻最多一张大图在解码。两条
/// 队列同为容量 1：在途水位 = 每队列一张排队 + worker 在手的各一张。
/// 大图的读盘与发送单独成一个任务——大图读得慢、大图队列又被慢解码
/// 顶住背压，混在一个发送循环里会周期性断掉小图的供给。
pub(super) async fn derive_missing<T, F>(
    missing: Vec<(String, PathBuf)>,
    compute: F,
) -> Result<Vec<(String, T)>, StoreError>
where
    T: Send + 'static,
    F: Fn(&[u8]) -> Option<T> + Send + Sync + Clone + 'static,
{
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // blob 按内容寻址、写入后不变，读头部即可在载入前完成分道。
    let mut small = Vec::with_capacity(missing.len());
    let mut large = Vec::new();
    for entry in missing {
        if is_large_blob(&entry.1) {
            large.push(entry);
        } else {
            small.push(entry);
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(4);
    let (small_tx, small_rx) = async_channel::bounded::<(String, Vec<u8>)>(1);
    let (large_tx, large_rx) = async_channel::bounded::<(String, Vec<u8>)>(1);
    let mut handles = Vec::with_capacity(workers + 1);
    for _ in 0..workers {
        handles.push(spawn_derive_worker(small_rx.clone(), compute.clone()));
    }
    handles.push(spawn_derive_worker(large_rx, compute));

    let large_sender = kovi::tokio::spawn(async move {
        for (hash, path) in large {
            let Ok(bytes) = kovi::tokio::fs::read(path).await else {
                continue;
            };
            // worker 崩溃才 send 失败，此时统一由下面的 JoinHandle 报错。
            let _ = large_tx.send((hash, bytes)).await;
        }
        // large_tx 随任务结束 drop，大图 worker 排空后自然退出。
    });

    for (hash, path) in small {
        let Ok(bytes) = kovi::tokio::fs::read(path).await else {
            continue;
        };
        let _ = small_tx.send((hash, bytes)).await;
    }
    drop(small_tx);

    large_sender
        .await
        .map_err(|e| StoreError::Other(anyhow::anyhow!("发送大图指纹任务失败: {e}")))?;
    let mut all = Vec::new();
    for handle in handles {
        let computed = handle
            .await
            .map_err(|e| StoreError::Other(anyhow::anyhow!("计算派生特征失败: {e}")))??;
        all.extend(computed);
    }
    Ok(all)
}
