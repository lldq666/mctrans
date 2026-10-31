//! mctrans-core：MC 整合包翻译核心库。
//!
//! 模块划分：
//! - `config`     配置加载/持久化（含翻译渠道选择）
//! - `text`       结构编辑器：JSON / SNBT / .lang，保形改写 + 编码探测
//! - `placeholder`占位符保护：掩码、校验、修复、兜底
//! - `providers`  翻译渠道抽象：DeepL、OpenAI 兼容，重试/限流/熔断/回退链
//! - `cache`      SQLite 缓存与运行状态（内容寻址 + 断点续跑）
//! - `pack`       整合包读写：zip/jar 流式处理、目录模式、目标规则
//! - `pipeline`   编排管线与报告

pub mod cache;
pub mod config;
pub mod pack;
pub mod pipeline;
pub mod placeholder;
pub mod providers;
pub mod report;
pub mod text;

pub use error::{McError, McResult};

mod error;

pub use config::Config;
