pub fn retry<F, T, E>(mut f: F, retries: usize) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let mut attempts = 0;
    loop {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
            }
        }
    }
}

pub async fn retry_async<F, Fut, T, E>(mut f: F, retries: usize) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
{
    let mut attempts = 0;
    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
            }
        }
    }
}
