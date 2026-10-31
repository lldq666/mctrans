//! 配置：mctrans.toml 加载、校验、翻译渠道选择的持久化。
//!
//! 渠道切换设计：
//! - `general.active_provider` 记录当前选中的翻译渠道；
//! - CLI `--provider X` 或 `mctrans config use X` 会**立即写入配置文件**（保留注释的文本级补丁），
//!   之后所有 run 默认使用该渠道，即"切换后保持选中状态并立即生效"；
//! - `fallback.chain` 定义选中渠道失败时的回退顺序。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{McError, McResult};
use crate::placeholder::PlaceholderCfg;

pub const DEFAULT_CONFIG_NAME: &str = "mctrans.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub providers: BTreeMap<String, ProviderCfg>,
    pub fallback: Fallback,
    pub placeholder: PlaceholderCfg,
    pub limits: Limits,
    pub targets: Vec<TargetRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            providers: BTreeMap::new(),
            fallback: Fallback::default(),
            placeholder: PlaceholderCfg::default(),
            limits: Limits::default(),
            targets: default_targets(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub src_lang: String,
    pub target_lang: String,
    /// zh_cn：新增中文语言文件；inplace：原地替换原语言文件。
    pub mode: TranslateMode,
    pub concurrency: usize,
    /// "auto" = 用户数据目录。
    pub db_path: String,
    pub cache: bool,
    /// 当前选中的翻译渠道名。
    pub active_provider: Option<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            src_lang: "en_us".into(),
            target_lang: "zh_cn".into(),
            mode: TranslateMode::ZhCn,
            concurrency: 8,
            db_path: "auto".into(),
            cache: true,
            active_provider: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateMode {
    ZhCn,
    Inplace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCfg {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub base_url: String,
    /// 从哪个环境变量读 Key；不配则按渠道类型用默认变量名。
    pub api_key_env: Option<String>,
    /// 明文 Key（仅本地调试用，会告警）。
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub formality: Option<String>,
    pub structured_output: Option<bool>,
    pub max_batch_texts: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Deepl,
    Openai,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Fallback {
    /// 回退顺序（渠道名）。
    pub chain: Vec<String>,
    /// 连续失败多少批后熔断该渠道（本 run 内不再使用）。
    pub circuit_breaker_failures: usize,
}

impl Default for Fallback {
    fn default() -> Self {
        Self {
            chain: Vec::new(),
            circuit_breaker_failures: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_batch_texts: usize,
    pub max_batch_chars: usize,
    pub max_entry_chars: usize,
    pub request_timeout_secs: u64,
    pub max_retries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_batch_texts: 40,
            max_batch_chars: 20_000,
            max_entry_chars: 8_000,
            request_timeout_secs: 60,
            max_retries: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetRule {
    pub name: String,
    pub kind: FileKind,
    /// glob 模式，`**` 跨目录，`*` 单层。
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    /// 仅 SnbtFields 使用：可译字段白名单。
    #[serde(default)]
    pub text_keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Json,
    Lang,
    SnbtMap,
    SnbtFields,
}

impl FileKind {
    /// 该格式是否强制 UTF-8。
    pub fn utf8_only(self) -> bool {
        matches!(self, FileKind::Json | FileKind::SnbtMap | FileKind::SnbtFields)
    }
}

pub fn default_targets() -> Vec<TargetRule> {
    vec![
        TargetRule {
            name: "mod-lang-json".into(),
            kind: FileKind::Json,
            patterns: vec![
                "mods/**/assets/*/lang/en_us.json".into(),
                "kubejs/assets/*/lang/en_us.json".into(),
                "resources/**/assets/*/lang/en_us.json".into(),
            ],
            text_keys: vec![],
        },
        TargetRule {
            name: "mod-lang-legacy".into(),
            kind: FileKind::Lang,
            patterns: vec!["mods/**/assets/*/lang/en_us.lang".into()],
            text_keys: vec![],
        },
        TargetRule {
            name: "ftb-quests-lang-snbt".into(),
            kind: FileKind::SnbtMap,
            patterns: vec!["config/ftbquests/quests/lang/*.snbt".into()],
            text_keys: vec![],
        },
        TargetRule {
            name: "ftb-quests-lang-json".into(),
            kind: FileKind::Json,
            patterns: vec!["config/ftbquests/quests/lang/*.json".into()],
            text_keys: vec![],
        },
        TargetRule {
            name: "ftb-quests-chapters".into(),
            kind: FileKind::SnbtFields,
            patterns: vec!["config/ftbquests/quests/chapters/**/*.snbt".into()],
            text_keys: vec![
                "title".into(),
                "subtitle".into(),
                "description".into(),
                "text".into(),
            ],
        },
    ]
}

impl Config {
    /// 加载配置；文件不存在时返回默认配置（providers 为空）。
    pub fn load(path: &Path) -> McResult<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path).map_err(|e| McError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|e| {
            McError::Config(format!(
                "解析 {} 失败: {e}\n（提示：检查 TOML 语法，字符串值需用双引号）",
                path.display()
            ))
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 按优先级找配置文件：--config 参数 > 当前目录 > 用户数据目录。
    pub fn locate(explicit: Option<&Path>) -> PathBuf {
        if let Some(p) = explicit {
            return p.to_path_buf();
        }
        let mut candidates = vec![PathBuf::from(DEFAULT_CONFIG_NAME)];
        candidates.push(user_data_dir().join(DEFAULT_CONFIG_NAME));
        for c in candidates {
            if c.exists() {
                return c;
            }
        }
        PathBuf::from(DEFAULT_CONFIG_NAME)
    }

    pub fn must_exist(&self) -> McResult<&Config> {
        Ok(self)
    }

    /// 解析渠道 Key：环境变量优先，其次配置里的明文 Key。
    pub fn resolve_api_key(&self, name: &str) -> Option<String> {
        let p = self.providers.get(name)?;
        let default_env = match p.kind {
            ProviderKind::Deepl => "DEEPL_API_KEY",
            ProviderKind::Openai => "OPENAI_API_KEY",
        };
        let env_name = p.api_key_env.as_deref().unwrap_or(default_env);
        if let Ok(v) = std::env::var(env_name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
        p.api_key.clone().filter(|v| !v.trim().is_empty())
    }

    /// 计算本次 run 的渠道顺序：选中渠道在最前，其余按 chain，最后是其余已配置渠道。
    pub fn provider_order(&self, cli_override: Option<&str>) -> Vec<String> {
        let known = |n: &String| self.providers.contains_key(n);
        let mut order: Vec<String> = Vec::new();
        let start = cli_override
            .map(str::to_string)
            .filter(known)
            .or_else(|| self.general.active_provider.clone().filter(known))
            .or_else(|| self.fallback.chain.first().cloned().filter(known))
            .or_else(|| self.providers.keys().next().cloned());
        if let Some(s) = start {
            order.push(s);
        }
        for c in &self.fallback.chain {
            if known(c) && !order.contains(c) {
                order.push(c.clone());
            }
        }
        for k in self.providers.keys() {
            if !order.contains(k) {
                order.push(k.clone());
            }
        }
        order
    }

    /// 当前生效渠道名（不含回退）。
    pub fn active_name(&self, cli_override: Option<&str>) -> Option<String> {
        cli_override
            .map(str::to_string)
            .or_else(|| self.general.active_provider.clone())
            .filter(|n| self.providers.contains_key(n))
            .or_else(|| self.providers.keys().next().cloned())
    }

    pub fn validate(&self) -> McResult<()> {
        if self.general.target_lang.trim().is_empty() || self.general.src_lang.trim().is_empty() {
            return Err(McError::Config("general.src_lang / target_lang 不能为空".into()));
        }
        if self.limits.max_batch_texts == 0 || self.limits.max_batch_chars == 0 {
            return Err(McError::Config("limits 的分批参数必须大于 0".into()));
        }
        for (name, p) in &self.providers {
            if p.base_url.trim().is_empty() {
                return Err(McError::Config(format!("渠道 {name} 缺少 base_url")));
            }
            if p.kind == ProviderKind::Openai && p.model.as_deref().unwrap_or("").trim().is_empty() {
                return Err(McError::Config(format!("OpenAI 兼容渠道 {name} 缺少 model")));
            }
        }
        Ok(())
    }
}

/// 用户数据目录（Windows: %APPDATA%/mctrans，其余: $HOME/.local/share/mctrans）。
pub fn user_data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("mctrans");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("share").join("mctrans");
    }
    PathBuf::from(".")
}

/// 解析数据库路径。
pub fn resolve_db_path(cfg: &Config) -> PathBuf {
    if cfg.general.db_path == "auto" || cfg.general.db_path.trim().is_empty() {
        user_data_dir().join("translations.db")
    } else {
        PathBuf::from(&cfg.general.db_path)
    }
}

/// 把"选中渠道"写入配置文件（文本级补丁，保留注释与其余字段）。
pub fn persist_active_provider(path: &Path, name: &str) -> McResult<()> {
    let existed = path.exists();
    let text = if existed {
        std::fs::read_to_string(path).map_err(|e| McError::Io {
            path: path.to_path_buf(),
            source: e,
        })?
    } else {
        String::new()
    };
    let new_line = format!("active_provider = \"{name}\"");
    let mut out: Vec<String> = Vec::new();
    let mut in_general = false;
    let mut patched = false;
    let mut general_seen = false;
    let ends_nl = text.is_empty() || text.ends_with('\n');
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // 离开 general 段时如果还没打补丁，就插在段头之后
            if in_general && !patched {
                out.push(new_line.clone());
                patched = true;
            }
            in_general = trimmed == "[general]";
            if in_general {
                general_seen = true;
            }
            out.push(line.to_string());
            continue;
        }
        if in_general && !patched && trimmed.starts_with("active_provider") {
            out.push(new_line.clone());
            patched = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !patched {
        if general_seen {
            out.push(new_line.clone());
        } else {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push("[general]".into());
            out.push(new_line);
        }
    }
    let mut result = out.join("\n");
    if ends_nl && !result.ends_with('\n') {
        result.push('\n');
    } else if result.is_empty() {
        result.push('\n');
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| McError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }
    std::fs::write(path, result).map_err(|e| McError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctrans-cfg-{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_example_config() {
        let d = tmp_dir("load");
        let p = d.join("mctrans.toml");
        fs::write(
            &p,
            r#"# 主配置
[general]
target_lang = "zh_cn"
active_provider = "deepl"

[providers.deepl]
type = "deepl"
base_url = "https://api.deepl.com"
api_key_env = "DEEPL_API_KEY"

[providers.gpt]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DS_API_KEY"
model = "deepseek-chat"

[fallback]
chain = ["deepl", "gpt"]
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.general.active_provider.as_deref(), Some("deepl"));
        assert_eq!(cfg.provider_order(None), vec!["deepl", "gpt"]);
        // CLI 覆盖优先
        assert_eq!(cfg.provider_order(Some("gpt")), vec!["gpt", "deepl"]);
        assert_eq!(cfg.active_name(None).as_deref(), Some("deepl"));
    }

    #[test]
    fn persist_provider_keeps_comments() {
        let d = tmp_dir("persist");
        let p = d.join("mctrans.toml");
        fs::write(
            &p,
            "# 头部注释\n[general]\ntarget_lang = \"zh_cn\"\n\n[providers.gpt]\ntype = \"openai\"\n",
        )
        .unwrap();
        persist_active_provider(&p, "gpt").unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# 头部注释"));
        assert!(text.contains("active_provider = \"gpt\""));
        assert!(text.contains("[providers.gpt]"));
        // 再切一次：替换而不是追加
        persist_active_provider(&p, "deepl").unwrap();
        let text2 = fs::read_to_string(&p).unwrap();
        assert!(text2.contains("active_provider = \"deepl\""));
        assert_eq!(text2.matches("active_provider").count(), 1);
    }

    #[test]
    fn persist_creates_general_section() {
        let d = tmp_dir("persist2");
        let p = d.join("mctrans.toml");
        fs::write(
            &p,
            "[placeholder]\nsentinel = \"@@{}@@\"\n\n[providers.gpt]\ntype = \"openai\"\nbase_url = \"https://api.deepseek.com/v1\"\nmodel = \"deepseek-chat\"\n",
        )
        .unwrap();
        persist_active_provider(&p, "gpt").unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("[general]"));
        assert!(text.contains("active_provider = \"gpt\""));
        assert!(text.contains("sentinel = \"@@{}@@\""));
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.provider_order(None), vec!["gpt"]);
    }

    #[test]
    fn default_config_has_targets() {
        let cfg = Config::default();
        assert!(!cfg.targets.is_empty());
        assert!(cfg.providers.is_empty());
        assert_eq!(cfg.limits.max_batch_texts, 40);
    }
}
