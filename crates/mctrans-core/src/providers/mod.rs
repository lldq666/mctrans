//! 翻译渠道抽象与回退链。
//!
//! - `Translator` trait：两个实现（DeepL、OpenAI 兼容）各自内部按自身限制分批；
//! - `ChainRunner`：按顺序尝试渠道，处理重试（指数退避+抖动）、限流（Retry-After）、
//!   熔断（连续失败 N 批后本 run 跳过）、TooLarge 二分；
//! - 渠道选择：由 config.general.active_provider / CLI --provider 决定，回退链兜底。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{ProviderCfg, ProviderKind};
use crate::error::{McError, McResult};

pub mod deepl;
pub mod openai;

/// 单次翻译请求上下文。
#[derive(Debug, Clone)]
pub struct ReqCtx {
    pub src_lang: String,
    pub tgt_lang: String,
    /// 追加到系统提示末尾的额外说明（如包名备注）。
    pub system_suffix: Option<String>,
    /// 术语表（已截断到预算内）。
    pub glossary: Vec<(String, String)>,
}

impl Default for ReqCtx {
    fn default() -> Self {
        Self {
            src_lang: "en_us".into(),
            tgt_lang: "zh_cn".into(),
            system_suffix: None,
            glossary: Vec::new(),
        }
    }
}

/// 渠道错误分类，驱动重试/回退策略。
#[derive(Debug, Clone, PartialEq)]
pub enum ErrKind {
    /// 可重试：网络抖动、5xx、译后校验失败。
    Retryable,
    /// 限流：按 retry_after 退避。
    RateLimited { retry_after: Option<Duration> },
    /// 批次/单条过大：调用方应二分后重试。
    TooLarge,
    /// 不可重试：鉴权失败、参数错误、渠道配置错误。
    Fatal,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub kind: ErrKind,
    pub msg: String,
}

impl ProviderError {
    pub fn fatal(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrKind::Fatal,
            msg: msg.into(),
        }
    }
    pub fn retryable(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrKind::Retryable,
            msg: msg.into(),
        }
    }
    pub fn too_large(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrKind::TooLarge,
            msg: msg.into(),
        }
    }
    pub fn rate_limited(msg: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            kind: ErrKind::RateLimited { retry_after },
            msg: msg.into(),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.msg)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub name: String,
    pub model: String,
    pub max_batch_texts: usize,
    pub max_batch_chars: usize,
}

/// 翻译渠道统一接口。实现方负责：按自身限制分批、错误分类。
/// 方法返回 Box<dyn Future> 以保持 dyn 兼容（Arc<dyn Translator>）。
pub trait Translator: Send + Sync {
    fn info(&self) -> ProviderInfo;
    fn translate<'a>(
        &'a self,
        batch: &'a [String],
        ctx: &'a ReqCtx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, ProviderError>> + Send + 'a>,
    >;
}

/// 最小请求间隔限速器（在并发之上再加一层平滑，避免触发 RPM 限制）。
#[derive(Default)]
pub struct IntervalLimiter {
    min: Duration,
    last: Mutex<Option<Instant>>,
}

impl IntervalLimiter {
    pub fn new(min: Duration) -> Self {
        Self {
            min,
            last: Mutex::new(None),
        }
    }

    pub async fn wait(&self) {
        if self.min.is_zero() {
            return;
        }
        let sleep_for = {
            let last = self.last.lock().unwrap();
            let now = Instant::now();
            match *last {
                Some(t) if now.duration_since(t) < self.min => Some(self.min - now.duration_since(t)),
                _ => None,
            }
        };
        if let Some(d) = sleep_for {
            tokio::time::sleep(d).await;
        }
        *self.last.lock().unwrap() = Some(Instant::now());
    }
}

/// 熔断器集合：渠道名 → 连续失败计数。
#[derive(Default)]
pub struct Breakers(Mutex<HashMap<String, AtomicUsize>>);

impl Breakers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, AtomicUsize>> {
        self.0.lock().unwrap()
    }
    pub fn is_open(&self, name: &str, threshold: usize) -> bool {
        self.lock()
            .get(name)
            .map(|c| c.load(Ordering::Relaxed) >= threshold)
            .unwrap_or(false)
    }
    pub fn record_failure(&self, name: &str) -> usize {
        let mut g = self.lock();
        let c = g.entry(name.to_string()).or_default();
        c.fetch_add(1, Ordering::Relaxed) + 1
    }
    pub fn record_success(&self, name: &str) {
        if let Some(c) = self.lock().get(name) {
            c.store(0, Ordering::Relaxed);
        }
    }
}

/// 简易抖动随机数（避免引入 rand 依赖）。
fn jitter(seed: u64) -> u64 {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    x ^= x >> 33;
    x
}

/// 按顺序执行渠道链。
pub struct ChainRunner {
    pub providers: Vec<(String, Arc<dyn Translator>)>,
    pub max_retries: usize,
    pub base_delay: Duration,
    pub breaker_threshold: usize,
    pub limiters: HashMap<String, Arc<IntervalLimiter>>,
}

/// 渠道执行结果：译文 + 实际使用的渠道名。
pub struct ChainOutcome {
    pub translations: Vec<String>,
    pub provider: String,
    /// 是否发生过重试/回退（用于报告）。
    pub retried: bool,
}

impl ChainRunner {
    /// 翻译一批文本。返回第一个成功的渠道结果；全部失败返回最后一次错误。
    pub async fn run(&self, batch: &[String], ctx: &ReqCtx, breakers: &Breakers) -> McResult<ChainOutcome> {
        if self.providers.is_empty() {
            return Err(McError::Config(
                "没有可用的翻译渠道：请检查配置里的 providers 与 active_provider".into(),
            ));
        }
        let mut last_err: Option<ProviderError> = None;
        let mut any_retry = false;
        for (name, provider) in &self.providers {
            if breakers.is_open(name, self.breaker_threshold) {
                tracing::warn!("渠道 {name} 已熔断，跳过");
                continue;
            }
            let mut attempt = 0usize;
            loop {
                if let Some(l) = self.limiters.get(name) {
                    l.wait().await;
                }
                let r = provider.translate(batch, ctx).await;
                match r {
                    Ok(t) => {
                        breakers.record_success(name);
                        return Ok(ChainOutcome {
                            translations: t,
                            provider: name.clone(),
                            retried: any_retry || attempt > 0,
                        });
                    }
                    Err(e) => match e.kind {
                        ErrKind::Retryable if attempt < self.max_retries => {
                            attempt += 1;
                            any_retry = true;
                            let d = self.base_delay * 2u32.pow(attempt.min(4) as u32);
                            let d = d + Duration::from_millis(jitter(attempt as u64 + 7) % 300);
                            tracing::warn!("渠道 {name} 第 {attempt} 次重试前退避 {d:?}: {}", e.msg);
                            tokio::time::sleep(d).await;
                        }
                        ErrKind::RateLimited { retry_after } if attempt < self.max_retries => {
                            attempt += 1;
                            any_retry = true;
                            let d = retry_after.unwrap_or(self.base_delay);
                            tracing::warn!("渠道 {name} 被限流，退避 {d:?}: {}", e.msg);
                            tokio::time::sleep(d).await;
                        }
                        ErrKind::TooLarge => {
                            // 二分由调用方处理；这里直接抛出，让上层拆批后重试。
                            return Err(McError::ProviderUnavailable {
                                provider: name.clone(),
                                reason: e.msg,
                            });
                        }
                        ErrKind::Fatal | ErrKind::Retryable | ErrKind::RateLimited { .. } => {
                            let fails = breakers.record_failure(name);
                            tracing::warn!(
                                "渠道 {name} 失败（连续 {fails} 次，上限 {}）: {}，尝试下一渠道",
                                self.breaker_threshold,
                                e.msg
                            );
                            last_err = Some(e);
                            break;
                        }
                    },
                }
            }
        }
        match last_err {
            Some(e) => Err(McError::ProviderUnavailable {
                provider: "chain".into(),
                reason: e.to_string(),
            }),
            None => Err(McError::Config(
                "所有渠道均已熔断或未配置：请检查 providers / active_provider / API Key".into(),
            )),
        }
    }
}

/// 构造渠道实例。
pub fn build_provider(name: &str, cfg: &ProviderCfg, api_key: Option<String>) -> McResult<Arc<dyn Translator>> {
    match cfg.kind {
        ProviderKind::Deepl => Ok(Arc::new(crate::providers::deepl::DeepLTranslator::new(
            name, cfg, api_key,
        )?)),
        ProviderKind::Openai => Ok(Arc::new(crate::providers::openai::OpenAiTranslator::new(
            name, cfg, api_key,
        )?)),
    }
}

/// 测试渠道连通性（config test 子命令用）。
pub async fn test_provider(provider: &Arc<dyn Translator>) -> McResult<String> {
    let ctx = ReqCtx::default();
    let out = provider
        .translate(&["Ping".to_string()], &ctx)
        .await
        .map_err(|e| McError::ProviderUnavailable {
            provider: provider.info().name.clone(),
            reason: e.to_string(),
        })?;
    Ok(out.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct MockProvider {
        name: String,
        calls: AtomicUsize,
        kind: ErrKind,
    }

    impl Translator for MockProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: self.name.clone(),
                model: "mock".into(),
                max_batch_texts: 50,
                max_batch_chars: 10_000,
            }
        }
        fn translate<'a>(
            &'a self,
            batch: &'a [String],
            _ctx: &'a ReqCtx,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<String>, ProviderError>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match &self.kind {
                    ErrKind::Fatal => Err(ProviderError::fatal("bad key")),
                    ErrKind::Retryable if self.calls.load(Ordering::SeqCst) == 1 => {
                        Err(ProviderError::retryable("temporary 500"))
                    }
                    ErrKind::RateLimited { .. } if self.calls.load(Ordering::SeqCst) <= 2 => {
                        Err(ProviderError::rate_limited("429", Some(Duration::from_millis(1))))
                    }
                    _ => Ok(batch.iter().map(|s| format!("[{s}]")).collect()),
                }
            })
        }
    }

    fn runner(providers: Vec<(String, Arc<dyn Translator>)>) -> ChainRunner {
        ChainRunner {
            providers,
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            breaker_threshold: 2,
            limiters: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn retry_then_success() {
        let p = Arc::new(MockProvider {
            name: "p".into(),
            calls: AtomicUsize::new(0),
            kind: ErrKind::Retryable,
        });
        let r = runner(vec![("p".into(), p.clone())]);
        let out = r.run(&["a".into()], &ReqCtx::default(), &Breakers::default()).await.unwrap();
        assert_eq!(out.translations, vec!["[a]"]);
        assert!(out.retried);
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fallback_to_second_provider() {
        let p1 = Arc::new(MockProvider {
            name: "p1".into(),
            calls: AtomicUsize::new(0),
            kind: ErrKind::Fatal,
        });
        let p2 = Arc::new(MockProvider {
            name: "p2".into(),
            calls: AtomicUsize::new(0),
            kind: ErrKind::Retryable,
        });
        let r = runner(vec![("p1".into(), p1), ("p2".into(), p2)]);
        let out = r.run(&["x".into()], &ReqCtx::default(), &Breakers::default()).await.unwrap();
        assert_eq!(out.provider, "p2");
        assert_eq!(out.translations, vec!["[x]"]);
    }

    #[tokio::test]
    async fn circuit_breaker_skips_provider() {
        let p = Arc::new(MockProvider {
            name: "bad".into(),
            calls: AtomicUsize::new(0),
            kind: ErrKind::Fatal,
        });
        let r = runner(vec![("bad".into(), p.clone())]);
        let b = Breakers::default();
        // 两次失败达到熔断阈值
        assert!(r.run(&["a".into()], &ReqCtx::default(), &b).await.is_err());
        assert!(r.run(&["a".into()], &ReqCtx::default(), &b).await.is_err());
        assert!(b.is_open("bad", 2));
        // 第三次执行应在进入渠道前就被熔断跳过（不再发起请求）
        let before = p.calls.load(Ordering::SeqCst);
        assert!(r.run(&["a".into()], &ReqCtx::default(), &b).await.is_err());
        assert_eq!(p.calls.load(Ordering::SeqCst), before);
    }

    #[tokio::test]
    async fn rate_limit_respects_retry_after() {
        let p = Arc::new(MockProvider {
            name: "rl".into(),
            calls: AtomicUsize::new(0),
            kind: ErrKind::RateLimited { retry_after: None },
        });
        let r = runner(vec![("rl".into(), p.clone())]);
        let out = r.run(&["a".into()], &ReqCtx::default(), &Breakers::default()).await.unwrap();
        assert_eq!(out.translations, vec!["[a]"]);
        assert_eq!(p.calls.load(Ordering::SeqCst), 3); // 2 次限流 + 1 次成功
    }
}
