//! 占位符保护：送译前把游戏内格式化占位符替换为唯一掩码，译后还原并校验。
//!
//! 保护对象（可在配置中开关/扩充）：
//! - MC 原版 § 颜色码（含 §x 十六进制颜色，逐段覆盖）
//! - Bukkit & 颜色码（&6 风格）
//! - printf 风格：%s %1$d %.2f %% 等
//! - MessageFormat：{0} {1,number}
//! - 转义序列：\n \t \r \\ \" \'
//! - 内联标签与 HTML 实体：<br> <b>…</b> &amp; &#65;
//! - 用户自定义正则（config: placeholder.custom_patterns）

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{McError, McResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaceholderCfg {
    /// 掩码模板，`{}` 处会被索引替换，如 "⟦{}⟧" → "⟦0⟧"。
    pub sentinel: String,
    /// 是否保护 &6 风格颜色码（可能误伤 "&amp;" 等实体，默认 true 且实体优先匹配）。
    pub amp_codes: bool,
    /// 译文中出现原文没有的新占位符时是否拒绝该译文（默认 true）。
    pub strict_new: bool,
    /// 用户自定义占位符正则（追加在内置规则之后，优先级最低）。
    pub custom_patterns: Vec<String>,
}

impl Default for PlaceholderCfg {
    fn default() -> Self {
        Self {
            sentinel: "⟦{}⟧".to_string(),
            amp_codes: true,
            strict_new: true,
            custom_patterns: Vec::new(),
        }
    }
}

/// 按优先级排列的占位符规则（优先级高者先匹配，处理 "&amp;" 这类重叠场景）。
struct Rule {
    #[allow(dead_code)]
    name: &'static str,
    re: Regex,
}

/// 编译后的占位符规则集。
pub struct PlaceholderEngine {
    rules: Vec<Rule>,
    mask_prefix: String,
    mask_suffix: String,
    mask_re: Regex,
    strict_new: bool,
}

/// 一次匹配记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub raw: String,
}

impl PlaceholderEngine {
    pub fn new(cfg: &PlaceholderCfg) -> McResult<Self> {
        let mut rules: Vec<Rule> = Vec::new();
        // 顺序即优先级：先具体后宽泛。
        rules.push(Rule {
            name: "escape",
            re: Regex::new(r#"\\[nrt\\"']"#).unwrap(),
        });
        rules.push(Rule {
            name: "msgfmt",
            re: Regex::new(r"\{\d+(?:,[^{}]*)?\}").unwrap(),
        });
        rules.push(Rule {
            name: "printf",
            re: Regex::new(r"%(\d+\$)?[-#+ 0,(]*\d*(\.\d+)?[sdifgeEuoxXcn%]").unwrap(),
        });
        rules.push(Rule {
            name: "tag",
            re: Regex::new(r"</?[A-Za-z][^>]*>|&[A-Za-z]+;|&#\d+;").unwrap(),
        });
        rules.push(Rule {
            name: "section",
            re: Regex::new("(?i)\u{a7}[0-9a-fk-orx]").unwrap(),
        });
        if cfg.amp_codes {
            rules.push(Rule {
                name: "amp",
                re: Regex::new("(?i)&[0-9a-fk-orx]").unwrap(),
            });
        }
        for (i, p) in cfg.custom_patterns.iter().enumerate() {
            rules.push(Rule {
                name: Box::leak(format!("custom{i}").into_boxed_str()),
                re: Regex::new(p).map_err(|e| McError::Config(format!("自定义占位符正则无效: {p} ({e})")))?,
            });
        }

        let (prefix, suffix) = match cfg.sentinel.split_once("{}") {
            Some((p, s)) => (p.to_string(), s.to_string()),
            None => (cfg.sentinel.clone(), String::new()),
        };
        // 掩码识别正则：prefix + 数字 + suffix，用于检测译文里的掩码残留/缺失。
        let mask_re = Regex::new(&format!(
            "{}([0-9]+){}",
            regex::escape(&prefix),
            regex::escape(&suffix)
        ))
        .map_err(|e| McError::Config(format!("sentinel 生成掩码正则失败: {e}")))?;

        Ok(Self {
            rules,
            mask_prefix: prefix,
            mask_suffix: suffix,
            mask_re,
            strict_new: cfg.strict_new,
        })
    }

    /// 找出文本中全部占位符（按出现顺序）。
    pub fn detect(&self, src: &str) -> Vec<Match> {
        let mut out = Vec::new();
        let bytes = src.as_bytes();
        let mut i = 0usize;
        while i < src.len() {
            let rest = &src[i..];
            let mut hit: Option<(usize, usize)> = None;
            for rule in &self.rules {
                if let Some(m) = rule.re.find(rest) {
                    if m.start() == 0 {
                        let end = m.end();
                        hit = Some((i, i + end));
                        break;
                    }
                }
            }
            match hit {
                Some((s, e)) => {
                    out.push(Match {
                        start: s,
                        end: e,
                        raw: src[s..e].to_string(),
                    });
                    i = e;
                }
                None => {
                    // 按字符步进，避免切坏 UTF-8。
                    let ch_len = utf8_len(bytes[i]);
                    i += ch_len;
                }
            }
        }
        out
    }

    fn mask_text(&self, idx: usize) -> String {
        format!("{}{}{}", self.mask_prefix, idx, self.mask_suffix)
    }

    /// 对原文建立守卫：记录全部占位符，生成对应掩码。
    pub fn guard(&self, src: &str) -> Guard<'_> {
        let matches = self.detect(src);
        let raws: Vec<String> = matches.iter().map(|m| m.raw.clone()).collect();
        let masks: Vec<String> = (0..raws.len()).map(|i| self.mask_text(i)).collect();
        Guard {
            engine: self,
            raws,
            masks,
        }
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 单个原文的占位符守卫。先 `mask()` 送译，再 `restore()` 还原并校验。
pub struct Guard<'a> {
    engine: &'a PlaceholderEngine,
    raws: Vec<String>,
    masks: Vec<String>,
}

impl<'a> Guard<'a> {
    /// 原文占位符列表（未掩码）。
    pub fn raws(&self) -> &[String] {
        &self.raws
    }

    /// 把原文中的占位符替换为掩码。
    pub fn mask(&self, src: &str) -> String {
        if self.raws.is_empty() {
            return src.to_string();
        }
        let matches = self.engine.detect(src);
        // detect 结果应与建守卫时一致（同一原文），否则按保守策略直接返回。
        if matches.len() != self.raws.len() {
            return src.to_string();
        }
        let mut out = String::with_capacity(src.len() + 16);
        let mut last = 0usize;
        for (m, mask) in matches.iter().zip(self.masks.iter()) {
            out.push_str(&src[last..m.start]);
            out.push_str(mask);
            last = m.end;
        }
        out.push_str(&src[last..]);
        out
    }

    /// 还原译文中的掩码，并做三重校验：
    /// 1. 每个掩码必须且只能出现一次，不允许出现未知掩码；
    /// 2. 还原后占位符多重集与原文一致（strict_new 开启时不允许译文引入新占位符）；
    /// 3. 首尾空白按原文补齐。
    pub fn restore(&self, translated: &str, src: &str) -> McResult<String> {
        // 1. 掩码完备性
        let mut counts = std::collections::HashMap::new();
        for c in self.engine.mask_re.captures_iter(translated) {
            let full = c.get(0).unwrap();
            let idx: usize = c[1].parse().unwrap_or(usize::MAX);
            if idx >= self.masks.len() {
                return Err(self.err(format!(
                    "译文出现未知掩码 {:?}（原文没有占位符 #{idx}）",
                    full.as_str()
                )));
            }
            *counts.entry(idx).or_insert(0usize) += 1;
        }
        for (i, mask) in self.masks.iter().enumerate() {
            match counts.get(&i).copied().unwrap_or(0) {
                0 => {
                    return Err(self.err(format!("掩码 {mask} 在译文中丢失")));
                }
                1 => {}
                n => {
                    return Err(self.err(format!("掩码 {mask} 在译文中出现 {n} 次")));
                }
            }
        }

        // 2. 还原
        let mut restored = translated.to_string();
        for (mask, raw) in self.masks.iter().zip(self.raws.iter()) {
            restored = restored.replacen(mask, raw, 1);
        }

        // 3. 多重集比对（防译文引入原文没有的占位符）
        if self.engine.strict_new {
            let before = multiset(&self.engine.detect(src));
            let after = multiset(&self.engine.detect(&restored));
            let mut extra: Vec<&String> = Vec::new();
            for (k, v) in after.iter() {
                let have = before.get(k).copied().unwrap_or(0);
                if v > &have {
                    for _ in 0..(v - have) {
                        extra.push(k);
                    }
                }
            }
            if !extra.is_empty() {
                return Err(self.err(format!(
                    "译文引入了原文没有的占位符: {:?}",
                    extra.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                )));
            }
        }

        // 4. 首尾空白按原文补齐（空白取自原文，核心取译文）
        let (lead, _, trail) = pad_split(src);
        let core = restored.trim_matches(|c: char| c.is_whitespace());
        let mut final_text = String::with_capacity(restored.len() + lead.len() + trail.len());
        final_text.push_str(lead);
        final_text.push_str(core);
        final_text.push_str(trail);
        Ok(final_text)
    }

    fn err(&self, msg: String) -> McError {
        McError::Placeholder {
            entry: self.raws.join(" | "),
            msg,
        }
    }
}

fn multiset(matches: &[Match]) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for x in matches {
        *m.entry(x.raw.clone()).or_insert(0usize) += 1;
    }
    m
}

/// 把字符串拆成 (首空白, 核心, 尾空白)，三段互不重叠。
fn pad_split(s: &str) -> (&str, &str, &str) {
    let is_ws = |c: char| c.is_whitespace();
    let trimmed = s.trim_matches(is_ws);
    let start = s.len() - s.trim_start_matches(is_ws).len();
    let end = start + trimmed.len();
    (&s[..start], &s[start..end], &s[end..])
}

/// 判断字符串是否值得送译：至少含一个拉丁字母，且不是纯符号/数字。
pub fn looks_translatable(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
}

/// 是否已包含 CJK 字符（说明可能已是中文）。
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F
            | 0xFF00..=0xFFEF | 0xF900..=0xFAFF)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> PlaceholderEngine {
        PlaceholderEngine::new(&PlaceholderCfg::default()).unwrap()
    }

    fn roundtrip(src: &str) -> String {
        let e = engine();
        let g = e.guard(src);
        let masked = g.mask(src);
        assert!(!masked.contains('\u{a7}') || !src.contains('\u{a7}'), "掩码后仍存在 §");
        g.restore(&masked, src).unwrap()
    }

    /// 模拟"模型在掩码之间插入译文"的场景。
    fn roundtrip_with(src: &str, translated: &str) -> String {
        let e = engine();
        let g = e.guard(src);
        g.restore(translated, src).unwrap()
    }

    #[test]
    fn roundtrip_is_identity() {
        for src in [
            "Get 10 Wood",
            "&6Cost: %1$s gold",
            "§aDone§r!",
            "Value {0} of {1,number}",
            "Line1\\nLine2",
            "<b>Bold</b> &amp; more",
            "§x§7§f§0§0§0§0Hex",
            "50%% done, %s left",
            "",
            "   ",
            "纯符号 !!! ???? 12345",
            "&6&lQuest &rcomplete",
        ] {
            assert_eq!(roundtrip(src), src, "roundtrip 不一致: {src:?}");
        }
    }

    #[test]
    fn entity_beats_amp_code() {
        let e = engine();
        // "&amp;" 必须整体作为实体保护，而不是拆成 "&a" + "mp;"
        let m = e.detect("Tom &amp; Jerry &6gold");
        assert_eq!(m[0].raw, "&amp;");
        assert_eq!(m[1].raw, "&6");
    }

    #[test]
    fn insert_translation_between_masks() {
        // src 的 &6 → ⟦0⟧，%1$s → ⟦1⟧；模型只替换中间文字
        let out = roundtrip_with("&6Cost: %1$s gold", "⟦0⟧花费：⟦1⟧ 黄金");
        assert_eq!(out, "&6花费：%1$s 黄金");
    }

    #[test]
    fn missing_mask_rejected() {
        let e = engine();
        let src = "A %s and {0}";
        let g = e.guard(src);
        let masked = g.mask(src);
        let broken = masked.replace("⟦1⟧", "丢了");
        assert!(g.restore(&broken, src).is_err());
    }

    #[test]
    fn duplicated_mask_rejected() {
        let e = engine();
        let src = "A %s and {0}";
        let g = e.guard(src);
        let masked = g.mask(src);
        let broken = masked.replacen("⟦1⟧", "⟦1⟧⟦1⟧", 1);
        assert!(g.restore(&broken, src).is_err());
    }

    #[test]
    fn unknown_mask_rejected() {
        let e = engine();
        let src = "A %s";
        let g = e.guard(src);
        let masked = g.mask(src);
        let broken = format!("{masked} 还有 ⟦3⟧");
        assert!(g.restore(&broken, src).is_err());
    }

    #[test]
    fn new_placeholder_in_translation_rejected() {
        let e = engine();
        let src = "Hello world";
        let g = e.guard(src);
        let masked = g.mask(src);
        let bad = "你好 %s 世界"; // 译文凭空引入 %s
        assert!(g.restore(bad, src).is_err());
        assert_eq!(g.restore("你好世界", src).unwrap(), "你好世界");
        let _ = masked;
    }

    #[test]
    fn whitespace_restored() {
        // 无占位符原文，直接模拟模型丢了首尾空白
        let e = engine();
        let g = e.guard("  leading and trailing  ");
        assert_eq!(g.restore("译文丢了空白", "  leading and trailing  ").unwrap(), "  译文丢了空白  ");
    }

    #[test]
    fn custom_patterns() {
        let cfg = PlaceholderCfg {
            custom_patterns: vec![r"#\[quest\]#".to_string()],
            ..Default::default()
        };
        let e = PlaceholderEngine::new(&cfg).unwrap();
        let src = "领取 #[quest]# 奖励";
        let g = e.guard(src);
        let masked = g.mask(src);
        assert_eq!(masked, "领取 ⟦0⟧ 奖励");
        assert_eq!(g.restore("领取 ⟦0⟧ 的奖励", src).unwrap(), "领取 #[quest]# 的奖励");
    }

    #[test]
    fn fuzz_roundtrip() {
        let e = engine();
        let pieces = [
            "plain text",
            "%s",
            "%1$d",
            "%.2f",
            "{0}",
            "{1,number}",
            "§6gold",
            "&lBold",
            "\\n",
            "<br>",
            "&amp;",
            "100%",
            "%%",
            "emoji 😀",
            "中文混排",
        ];
        let mut seed = 12345u64;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..300 {
            let n = rnd() % 8 + 1;
            let src: String = (0..n).map(|_| pieces[rnd() % pieces.len()]).collect::<Vec<_>>().join(" ");
            let g = e.guard(&src);
            let masked = g.mask(&src);
            let back = g.restore(&masked, &src).unwrap();
            assert_eq!(back, src);
        }
    }

    #[test]
    fn skip_heuristics() {
        assert!(!looks_translatable("12345"));
        assert!(!looks_translatable("---"));
        assert!(looks_translatable("abc"));
        assert!(has_cjk("已有中文"));
        assert!(!has_cjk("english only"));
    }
}
