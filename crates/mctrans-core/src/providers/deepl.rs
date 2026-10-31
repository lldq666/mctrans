//! DeepL v2 API 渠道。

use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::ProviderCfg;
use crate::error::McResult;

use super::{ErrKind, ProviderError, ProviderInfo, ReqCtx, Translator};

pub struct DeepLTranslator {
    name: String,
    base_url: String,
    api_key: String,
    formality: String,
    max_batch_texts: usize,
    max_batch_chars: usize,
    client: Client,
}

#[derive(Serialize)]
struct TranslateReq {
    text: Vec<String>,
    source_lang: String,
    target_lang: String,
    formality: String,
    preserve_formatting: bool,
}

#[derive(Deserialize)]
struct TranslateResp {
    translations: Vec<TransItem>,
}

#[derive(Deserialize)]
struct TransItem {
    text: String,
}

const MAX_BATCH: usize = 50; // DeepL 单批上限
const MAX_REQ_BYTES: usize = 128 * 1024;

impl DeepLTranslator {
    pub fn new(name: &str, cfg: &ProviderCfg, api_key: Option<String>) -> McResult<Self> {
        let key = api_key.ok_or_else(|| {
            crate::error::McError::Config(format!(
                "渠道 {name} 缺少 API Key：请设置环境变量（默认 DEEPL_API_KEY，或用 api_key_env 指定）"
            ))
        })?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| crate::error::McError::Config(format!("构造 HTTP 客户端失败: {e}")))?;
        Ok(Self {
            name: name.to_string(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: key,
            formality: cfg.formality.clone().unwrap_or_else(|| "default".into()),
            max_batch_texts: cfg.max_batch_texts.unwrap_or(MAX_BATCH).min(MAX_BATCH),
            max_batch_chars: 100_000,
            client,
        })
    }

    /// DeepL 语种代码：en → EN，zh_cn → ZH（简体）/ ZH-HANT。
    fn map_lang(lang: &str) -> String {
        let l = lang.to_lowercase().replace('-', "_");
        match l.as_str() {
            "zh_cn" | "zh" => "ZH".into(),
            "zh_tw" | "zh_hk" | "zh_hant" => "ZH-HANT".into(),
            _ => l.split('_').next().unwrap_or("EN").to_uppercase(),
        }
    }
}

impl Translator for DeepLTranslator {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: self.name.clone(),
            model: "deepl-v2".into(),
            max_batch_texts: self.max_batch_texts,
            max_batch_chars: self.max_batch_chars,
        }
    }

    fn translate<'a>(
        &'a self,
        batch: &'a [String],
        ctx: &'a ReqCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(batch.len());
        // 按数量/字节两个维度切块
        let mut chunks: Vec<&[String]> = Vec::new();
        let mut start = 0usize;
        while start < batch.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < batch.len()
                && end - start < self.max_batch_texts
                && bytes + batch[end].len() < MAX_REQ_BYTES
            {
                bytes += batch[end].len();
                end += 1;
            }
            if end == start {
                end = start + 1; // 单条就超限：仍然单独发一次，让服务端裁决
            }
            chunks.push(&batch[start..end]);
            start = end;
        }
        for chunk in chunks {
            let req = TranslateReq {
                text: chunk.to_vec(),
                source_lang: Self::map_lang(&ctx.src_lang),
                target_lang: Self::map_lang(&ctx.tgt_lang),
                formality: self.formality.clone(),
                preserve_formatting: false,
            };
            let resp = self
                .client
                .post(format!("{}/v2/translate", self.base_url))
                .header("Authorization", format!("DeepL-Auth-Key {}", self.api_key))
                .json(&req)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        ProviderError::rate_limited("请求超时", None)
                    } else {
                        ProviderError::retryable(format!("网络错误: {e}"))
                    }
                })?;
            let status = resp.status();
            if !status.is_success() {
                let retry_after = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v: &HeaderValue| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs);
                let body = resp.text().await.unwrap_or_default();
                let msg = format!("HTTP {} {}", status.as_u16(), truncate(&body, 300));
                let kind = match status.as_u16() {
                    401 | 403 => ErrKind::Fatal,
                    429 => ErrKind::RateLimited { retry_after },
                    413 => ErrKind::TooLarge,
                    456 => ErrKind::Fatal, // 额度用尽：切换渠道而非重试
                    408 | 425 | 529 => ErrKind::Retryable,
                    s if s >= 500 => ErrKind::Retryable,
                    _ => ErrKind::Fatal,
                };
                return Err(ProviderError { kind, msg });
            }
            let parsed: TranslateResp = resp.json().await.map_err(|e| {
                ProviderError::retryable(format!("响应解析失败: {e}"))
            })?;
            if parsed.translations.len() != chunk.len() {
                return Err(ProviderError::retryable(format!(
                    "返回条数不符：期望 {}，实得 {}",
                    chunk.len(),
                    parsed.translations.len()
                )));
            }
            out.extend(parsed.translations.into_iter().map(|t| t.text));
        }
        Ok(out)
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_mapping() {
        assert_eq!(DeepLTranslator::map_lang("en_us"), "EN");
        assert_eq!(DeepLTranslator::map_lang("zh_cn"), "ZH");
        assert_eq!(DeepLTranslator::map_lang("zh_tw"), "ZH-HANT");
    }

    #[tokio::test]
    async fn rejects_missing_key() {
        let cfg = ProviderCfg {
            kind: crate::config::ProviderKind::Deepl,
            base_url: "https://api.deepl.com".into(),
            api_key_env: None,
            api_key: None,
            model: None,
            temperature: None,
            formality: None,
            structured_output: None,
            max_batch_texts: None,
        };
        assert!(DeepLTranslator::new("deepl", &cfg, None).is_err());
    }
}
