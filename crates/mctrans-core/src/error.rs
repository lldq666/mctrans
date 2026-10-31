use std::path::PathBuf;

/// 全库统一错误类型。每个变体尽量携带定位信息（路径、条目、原文摘要），
/// 最终由 pipeline 汇总进运行报告。
#[derive(Debug, thiserror::Error)]
pub enum McError {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("IO 错误 ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("压缩包错误 ({path}): {source}")]
    Zip {
        path: PathBuf,
        #[source]
        source: zip::result::ZipError,
    },

    #[error("JSON 解析错误 ({path}): {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("数据库错误: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("文件解码失败 ({path}): {msg}")]
    Decode { path: String, msg: String },

    #[error("占位符校验失败 ({entry}): {msg}")]
    Placeholder { entry: String, msg: String },

    #[error("翻译渠道不可用 ({provider}): {reason}")]
    ProviderUnavailable { provider: String, reason: String },

    #[error("翻译渠道限流 ({provider}): {msg}")]
    RateLimited { provider: String, msg: String },

    #[error("输出被占用 ({path}): 可能有另一个进程正在写同一目标")]
    OutputLocked { path: PathBuf },

    #[error("运行被取消")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

pub type McResult<T> = Result<T, McError>;

/// 构造 IO 错误的便捷函数。
pub fn io_err(path: impl Into<PathBuf>, source: std::io::Error) -> McError {
    McError::Io {
        path: path.into(),
        source,
    }
}
