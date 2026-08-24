use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tracing::info;

/// 答案池（2315 词）：cfreshman 的 gist，官方 NYT 词表镜像。
/// gist 不支持 jsDelivr 镜像，故无备源；失败时允许手动预置文件。
const ANSWERS_URL: &str = "https://gist.githubusercontent.com/cfreshman/a03ef2cba789d8cf00c08f767e0fad7b/raw/wordle-answers-alphabetical.txt";

/// 可猜池全表（14854 词）：tabatkins/wordle-list 镜像的官方全词表。
/// 备源走 jsDelivr CDN，避免 GitHub raw 不可达时下载失败。
const ALLOWED_URL: &str = "https://raw.githubusercontent.com/tabatkins/wordle-list/main/words";
const ALLOWED_URL_FALLBACK: &str = "https://cdn.jsdelivr.net/gh/tabatkins/wordle-list@main/words";

const ANSWERS_FILE: &str = "answers.txt";
const ALLOWED_FILE: &str = "allowed.txt";

/// 词库文件解析后的最小词数，低于此值视为文件损坏。
const MIN_ANSWERS: usize = 1000;
const MIN_ALLOWED: usize = 10_000;

/// 单次下载响应体上限，防止异常源返回超大内容。
const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024;

/// 词库：`answers` 是答案池，`allowed` 是允许猜测的全集（含答案词）。
#[derive(Debug)]
pub struct WordList {
    pub answers: Vec<String>,
    pub allowed: HashSet<String>,
}

/// 确保词库就绪：本地缓存缺失时按 URL 顺序尝试下载，成功写入缓存。
///
/// 缓存文件位于 `data_dir/wordle/`，解析后不合法则报错并提示手动预置。
pub async fn load_or_download(data_dir: &Path) -> anyhow::Result<WordList> {
    let word_dir = data_dir.join("wordle");
    fs::create_dir_all(&word_dir)
        .with_context(|| format!("创建词库目录 {} 失败", word_dir.display()))?;

    let answers_path = word_dir.join(ANSWERS_FILE);
    ensure_file(&answers_path, &[ANSWERS_URL]).await?;
    let allowed_path = word_dir.join(ALLOWED_FILE);
    ensure_file(&allowed_path, &[ALLOWED_URL, ALLOWED_URL_FALLBACK]).await?;

    let answers = parse_word_file(&answers_path)
        .with_context(|| format!("解析 {} 失败", answers_path.display()))?;
    let allowed = parse_word_file(&allowed_path)
        .with_context(|| format!("解析 {} 失败", allowed_path.display()))?;

    if answers.len() < MIN_ANSWERS {
        bail!(
            "答案池过小（{} 词，预期至少 {MIN_ANSWERS}），请删除 {} 后重试",
            answers.len(),
            answers_path.display()
        );
    }
    if allowed.len() < MIN_ALLOWED {
        bail!(
            "可猜池过小（{} 词，预期至少 {MIN_ALLOWED}），请删除 {} 后重试",
            allowed.len(),
            allowed_path.display()
        );
    }

    // 允许猜测 = 全表 ∪ 答案池，兼容官方"答案在可猜池内"的规则。
    let allowed: HashSet<String> = allowed.into_iter().chain(answers.iter().cloned()).collect();
    Ok(WordList { answers, allowed })
}

/// 文件存在且非空则直接复用，否则按顺序尝试每个 URL 下载。
async fn ensure_file(path: &Path, urls: &[&str]) -> anyhow::Result<()> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => {
            download_to_file(urls, path).await?;
            return Ok(());
        }
    };
    if meta.len() == 0 {
        // 空文件视为损坏缓存，重下覆盖。
        download_to_file(urls, path).await?;
    }
    Ok(())
}

/// 依次尝试各 URL，全部失败才报错；下载内容先写临时文件再原子改名，
/// 避免下载中断留下半截文件被当作有效缓存。
async fn download_to_file(urls: &[&str], path: &Path) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("wordle-cli")
        .build()
        .context("构建 HTTP 客户端失败")?;

    let mut last_err: Option<anyhow::Error> = None;
    for url in urls {
        info!("下载词库: {url}");
        match download_one(&client, url, path).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!("下载 {url} 失败: {err:#}");
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("没有可用的词库下载源")))
}

async fn download_one(client: &reqwest::Client, url: &str, path: &Path) -> anyhow::Result<()> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?
        .error_for_status()
        .with_context(|| format!("{url} 返回错误状态"))?;
    if let Some(len) = resp.content_length()
        && len > MAX_DOWNLOAD_BYTES
    {
        bail!("{url} 响应体过大（{len} 字节）");
    }

    // 流式读取并逐块检查上限，chunked 响应同样受保护。
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("读取 {url} 响应失败"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > MAX_DOWNLOAD_BYTES {
            bail!("{url} 响应体过大（{} 字节）", body.len());
        }
    }

    let tmp = tmp_path(path);
    if let Err(err) = fs::write(&tmp, &body).and_then(|()| fs::rename(&tmp, path)) {
        // 失败时清掉半截临时文件，避免污染后续重试。
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("写入 {} 失败", path.display()));
    }
    info!("词库已缓存到 {}", path.display());
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}

/// 解析词库文件：每行一词，转小写后只保留 5 个 a-z 字母的词。
fn parse_word_file(path: &Path) -> anyhow::Result<Vec<String>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut words = Vec::new();
    for line in content.lines() {
        let word = line.trim().to_ascii_lowercase();
        if is_valid_word(&word) {
            words.push(word);
        }
    }
    words.sort();
    words.dedup();
    Ok(words)
}

fn is_valid_word(word: &str) -> bool {
    word.len() == 5 && word.bytes().all(|b| b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    fn write_words(dir: &Path, file: &str, words: impl IntoIterator<Item = impl AsRef<str>>) {
        let content: Vec<String> = words.into_iter().map(|w| w.as_ref().to_owned()).collect();
        fs::write(dir.join(file), content.join("\n")).unwrap();
    }

    #[test]
    fn parses_and_filters_words() {
        let dir = tempfile_dir();
        write_words(
            &dir,
            ALLOWED_FILE,
            ["crane", "CRANE", "hello!", "ab", "abcde", "abcde", ""],
        );
        let words = parse_word_file(&dir.join(ALLOWED_FILE)).unwrap();
        assert_eq!(words, vec!["abcde".to_owned(), "crane".to_owned()]);
        remove_dir(&dir);
    }

    #[tokio::test]
    async fn load_reads_cached_files_without_download() {
        let dir = tempfile_dir();
        let word_dir = dir.join("wordle");
        fs::create_dir_all(&word_dir).unwrap();
        // 用 base-26 编码生成唯一的纯字母 5 词（is_valid_word 只接受 a-z）
        let answers: Vec<String> = (0..2000).map(fake_word).collect();
        let mut allowed: Vec<String> = (0..12_000).map(fake_word).collect();
        // 让答案词也在 allowed 中出现，验证并集去重后仍满足数量下限
        allowed.extend(answers.iter().cloned());
        write_words(&word_dir, ANSWERS_FILE, &answers);
        write_words(&word_dir, ALLOWED_FILE, &allowed);

        let list = load_or_download(&dir).await.unwrap();
        assert!(list.answers.len() >= MIN_ANSWERS);
        assert!(list.allowed.len() >= MIN_ALLOWED);
        for a in &list.answers {
            assert!(list.allowed.contains(a), "{a} 应可被猜测");
        }
        remove_dir(&dir);
    }

    #[tokio::test]
    async fn rejects_corrupt_word_lists() {
        let dir = tempfile_dir();
        let word_dir = dir.join("wordle");
        fs::create_dir_all(&word_dir).unwrap();
        write_words(&word_dir, ANSWERS_FILE, ["abcde"]);
        write_words(&word_dir, ALLOWED_FILE, ["abcde"]);

        let err = load_or_download(&dir).await.unwrap_err();
        assert!(err.to_string().contains("答案池过小"), "{err:#}");
        remove_dir(&dir);
    }

    fn fake_word(i: usize) -> String {
        let mut i = i;
        let mut s = String::new();
        for _ in 0..5 {
            s.push((b'a' + (i % 26) as u8) as char);
            i /= 26;
        }
        s
    }

    fn remove_dir(dir: &Path) {
        fs::remove_dir_all(dir).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wordle-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
