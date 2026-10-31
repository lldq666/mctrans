//! 运行报告：JSON + Markdown。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunReport {
    pub pack: String,
    pub output: String,
    pub src_lang: String,
    pub tgt_lang: String,
    pub mode: String,
    pub dry_run: bool,
    pub active_provider: String,
    pub provider_order: Vec<String>,
    pub files_total: usize,
    pub files_translated: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub units_total: usize,
    pub cache_hits: usize,
    pub translated: usize,
    pub would_translate: usize,
    pub placeholder_failures: usize,
    pub too_long: usize,
    pub duration_ms: u128,
    pub provider_usage: Vec<ProviderUsage>,
    pub failures: Vec<FailureEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub provider: String,
    pub batches: usize,
    pub chars_in: usize,
    pub chars_out: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailureEntry {
    pub container: String,
    pub unit: String,
    pub source: String,
    pub reason: String,
}

impl RunReport {
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str("# mctrans 运行报告\n\n");
        s.push_str(&format!("- 整合包：`{}`\n", self.pack));
        s.push_str(&format!("- 输出：`{}`\n", self.output));
        s.push_str(&format!(
            "- 方向：`{} → {}`（模式 {}）\n",
            self.src_lang, self.tgt_lang, self.mode
        ));
        s.push_str(&format!(
            "- 当前翻译渠道：**{}**（回退顺序：{}）\n",
            self.active_provider,
            self.provider_order.join(" → ")
        ));
        if self.dry_run {
            s.push_str("- ⚠️ dry-run：未写入任何文件\n");
        }
        s.push_str(&format!("- 耗时：{} ms\n\n", self.duration_ms));
        s.push_str("## 统计\n\n");
        s.push_str("| 指标 | 数量 |\n|---|---|\n");
        s.push_str(&format!("| 扫描文件 | {} |\n", self.files_total));
        s.push_str(&format!("| 翻译文件 | {} |\n", self.files_translated));
        s.push_str(&format!("| 跳过文件 | {} |\n", self.files_skipped));
        s.push_str(&format!("| 失败文件 | {} |\n", self.files_failed));
        s.push_str(&format!("| 待译单元 | {} |\n", self.units_total));
        s.push_str(&format!("| 缓存命中 | {} |\n", self.cache_hits));
        s.push_str(&format!("| 实际翻译 | {} |\n", self.translated));
        s.push_str(&format!("| 占位符保护失败（保留原文） | {} |\n", self.placeholder_failures));
        s.push_str(&format!("| 超长跳过 | {} |\n\n", self.too_long));
        if !self.provider_usage.is_empty() {
            s.push_str("## 渠道用量\n\n| 渠道 | 批次数 | 输入字符 | 输出字符 |\n|---|---|---|---|\n");
            for u in &self.provider_usage {
                s.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    u.provider, u.batches, u.chars_in, u.chars_out
                ));
            }
            s.push('\n');
        }
        if !self.failures.is_empty() {
            s.push_str("## 失败明细\n\n| 文件 | 单元 | 原因 |\n|---|---|---|\n");
            for f in self.failures.iter().take(200) {
                s.push_str(&format!(
                    "| `{}` | `{}` | {} |\n",
                    escape_md(&f.container),
                    escape_md(&f.unit),
                    escape_md(&truncate(&f.reason, 160))
                ));
            }
            s.push('\n');
        }
        s
    }
}

fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}
