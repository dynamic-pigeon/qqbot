#[derive(thiserror::Error, Debug)]
pub enum BvError {
    #[error("请求失败: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("解析返回体失败: {0}")]
    RequestBodyError(String),
    #[error("解析失败: {0}")]
    ParseFailed(&'static str),
    #[error("其他错误: {0}")]
    Other(String),
}
