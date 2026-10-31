//! OpenAI 兼容渠道（chat/completions）：OpenAI、DeepSeek、Moonshot、SiliconFlow、本地 Ollama 等。

use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::ProviderCfg;
use crate::error::{McError, McResult};

use super::{ErrKind, ProviderError, ProviderInfo, ReqCtx, Translator};

pub struct OpenAiTranslator {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    temperature: f32,
    structured_output: bool,
    max_batch_texts: usize,
    client: Client,
}

#[derive(Serialize)]
struct ChatReq {
    model: String,
    temperature: f32,
    messages: Vec<Msg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
}

#[derive(Serialize, Deserialize)]
struct Msg {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Msg,
}

const SYSTEM_PROMPT: &str = "你是 Minecraft 模组整合包的简体中文本地化翻译引擎。严格遵守：\n\
1. 只翻译用户消息 JSON 数组中的字符串，输出数组必须与输入等长、同序；\n\
2. 形如 ⟦0⟧ 的标记是受保护占位符，译文中必须原样保留且各出现恰好一次，不得增删改；\n\
3. 保留原文首尾空白与换行结构；\n\
4. 使用简体中文，采用游戏圈通用译名（如 Efficiency=效率，Diamond=钻石）；\n\
5. 术语表（如有）优先遵守；\n\
6. 只输出 JSON 字符串数组本身：不要解释、不要 Markdown 代码围栏、不要多余字段。";

impl OpenAiTranslator {
    pub fn new(name: &str, cfg: &ProviderCfg, api_key: Option<String>) -> McResult<Self> {
        let key = api_key.unwrap_or_else(|| "not-needed".into());
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| McError::Config(format!("构造 HTTP 客户端失败: {e}")))?;
        Ok(Self {
            name: name.to_string(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: key,
            model: cfg.model.clone().unwrap_or_else(|| "gpt-4o-mini".into()),
            temperature: cfg.temperature.unwrap_or(0.0),
            structured_output: cfg.structured_output.unwrap_or(false),
            max_batch_texts: cfg.max_batch_texts.unwrap_or(40),
            client,
        })
    }

    fn build_system(&self, ctx: &ReqCtx) -> String {
        let mut s = SYSTEM_PROMPT.to_string();
        if !ctx.glossary.is_empty() {
            s.push_str("\n术语表：");
            for (k, v) in ctx.glossary.iter().take(30) {
                s.push_str(&format!("\n{k} ⇒ {v}"));
            }
        }
        if let Some(suffix) = &ctx.system_suffix {
            s.push_str("\n附加说明：");
            s.push_str(suffix);
        }
        s
    }
}

impl Translator for OpenAiTranslator {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: self.name.clone(),
            model: self.model.clone(),
            max_batch_texts: self.max_batch_texts,
            max_batch_chars: 24_000,
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
        let user_content = format!(
            "把下面的游戏文本翻译成{}。只输出等长 JSON 字符串数组。\n{}",
            tgt_display(&ctx.tgt_lang),
            serde_json::to_string(batch).unwrap()
        );
        let req = ChatReq {
            model: self.model.clone(),
            temperature: self.temperature,
            messages: vec![
                Msg {
                    role: "system".into(),
                    content: self.build_system(ctx),
                },
                Msg {
                    role: "user".into(),
                    content: user_content,
                },
            ],
            response_format: if self.structured_output {
                Some(json!({"type": "json_object"}))
            } else {
                None
            },
        };
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
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
                .and_then(parse_secs)
                .or_else(|| {
                    resp.headers()
                        .get("x-ratelimit-reset-requests")
                        .and_then(|v: &HeaderValue| v.to_str().ok())
                        .and_then(parse_secs)
                });
            let body = resp.text().await.unwrap_or_default();
            let detail = extract_error_message(&body);
            let msg = format!("HTTP {} {}", status.as_u16(), truncate(&detail, 300));
            let kind = match status.as_u16() {
                401 | 403 => ErrKind::Fatal,
                404 => ErrKind::Fatal,
                408 | 425 | 529 => ErrKind::Retryable,
                429 => ErrKind::RateLimited { retry_after },
                400 if is_context_length(&detail) => ErrKind::TooLarge,
                s if s >= 500 => ErrKind::Retryable,
                _ => ErrKind::Fatal,
            };
            return Err(ProviderError { kind, msg });
        }
        let parsed: ChatResp = resp
            .json()
            .await
            .map_err(|e| ProviderError::retryable(format!("响应解析失败: {e}")))?;
        let content = parsed
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .ok_or_else(|| ProviderError::retryable("响应缺少 choices"))?;
        match parse_array_lenient(&content) {
            Ok(items) if items.len() == batch.len() => Ok(items),
            Ok(items) => Err(ProviderError::retryable(format!(
                "返回条数不符：期望 {}，实得 {}",
                batch.len(),
                items.len()
            ))),
            Err(e) => Err(ProviderError::retryable(format!("译文 JSON 解析失败: {e}"))),
        }
        })
    }
}

fn tgt_display(lang: &str) -> String {
    match lang.to_lowercase().replace('-', "_").as_str() {
        "zh_cn" | "zh" => "简体中文".into(),
        "zh_tw" | "zh_hk" | "zh_hant" => "繁体中文".into(),
        other => other.to_string(),
    }
}

fn parse_secs(s: &str) -> Option<Duration> {
    // 兼容 "3"、"3s"、"0.5"
    let t = s.trim().trim_end_matches('s');
    if let Ok(sec) = t.parse::<f64>() {
        if sec.is_finite() && sec >= 0.0 {
            return Some(Duration::from_secs_f64(sec.min(30.0)));
        }
    }
    None
}

fn is_context_length(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("context length") || m.contains("maximum context") || m.contains("too many tokens")
}

fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = v
            .pointer("/error/message")
            .or_else(|| v.pointer("/message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
    }
    body.to_string()
}

/// 宽松 JSON 数组解析：直接解析 → 剥代码围栏 → 截取 [..] → 宽松映射元素。
pub fn parse_array_lenient(content: &str) -> McResult<Vec<String>> {
    let trimmed = content.trim();
    // 去掉 Markdown 代码围栏（模型常见输出形态）
    let work: String = if trimmed.starts_with("```") {
        let mut out = String::new();
        for (i, l) in trimmed.lines().enumerate() {
            if i == 0 || l.trim_start().starts_with("```") {
                continue;
            }
            out.push_str(l);
            out.push('\n');
        }
        out
    } else {
        trimmed.to_string()
    };
    let s = work.trim();
    // 直接解析
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return items_from_value(&v);
    }
    // 截取首个 [ 到末个 ]
    if let (Some(a), Some(b)) = (s.find('['), s.rfind(']')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<Value>(&s[a..=b]) {
                return items_from_value(&v);
            }
        }
    }
    Err(McError::Other(format!(
        "无法从模型输出中解析 JSON 数组: {}",
        truncate(content, 200)
    )))
}

fn items_from_value(v: &Value) -> McResult<Vec<String>> {
    match v {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    Value::Object(o) => {
                        let s = o
                            .get("text")
                            .or_else(|| o.get("translation"))
                            .and_then(|t| t.as_str())
                            .ok_or_else(|| McError::Other("数组元素既不是字符串也没有 text 字段".into()))?;
                        out.push(s.to_string());
                    }
                    Value::Number(n) => out.push(n.to_string()),
                    other => {
                        return Err(McError::Other(format!("数组元素类型不支持: {other}")));
                    }
                }
            }
            Ok(out)
        }
        other => Err(McError::Other(format!("期望 JSON 数组，实得: {other}"))),
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
    fn lenient_parse_variants() {
        assert_eq!(
            parse_array_lenient(r#"["a","b"]"#).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_array_lenient("```json\n[\"a\",\n \"b\"]\n```").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_array_lenient("好的，结果如下：[{\"text\":\"a\"},{\"text\":\"b\"}]").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_array_lenient("完全没有数组").is_err());
    }

    #[test]
    fn error_classification() {
        assert!(is_context_length("This model's maximum context length is 8192 tokens"));
        assert!(!is_context_length("invalid api key"));
        assert_eq!(parse_secs("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_secs("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_secs("nan"), None);
    }

    #[test]
    fn extract_error_variants() {
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"bad key","type":"auth"}}"#),
            "bad key"
        );
        assert_eq!(extract_error_message("plain text"), "plain text");
    }
}
