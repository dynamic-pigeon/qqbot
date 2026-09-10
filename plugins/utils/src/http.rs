use std::{sync::LazyLock, time::Duration};

/// 进程级 HTTP 客户端：复用连接池，禁止跟随 redirect，避免公网域名跳到内网。
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .build()
            .expect("hardcoded reqwest client configuration must be valid")
    });
    &CLIENT
}
