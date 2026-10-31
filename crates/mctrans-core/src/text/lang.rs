//! legacy .lang 文件（Java Properties 风格）：按行处理，保留注释、空行、键顺序。
//! 转义规则对齐 java.util.Properties：\n \t \r \f \\ \= \: \# \! \空格 \uXXXX。

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum LangLine {
    /// 注释行 / 空行 / 无法解析的行，原样保留。
    Other(String),
    /// key=value 条目；raw 是去掉行尾符的原始文本，值未变化时按 raw 原样输出。
    Entry {
        key: String,
        value: String,
        raw: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct LangFile {
    pub lines: Vec<LangLine>,
    pub newline: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'f' => out.push('\u{000C}'),
                b'\\' => out.push('\\'),
                b'=' => out.push('='),
                b':' => out.push(':'),
                b'#' => out.push('#'),
                b'!' => out.push('!'),
                b'u' if i + 4 < b.len() => {
                    if let Ok(cp) = u32::from_str_radix(&s[i + 1..i + 5], 16) {
                        if let Some(c) = char::from_u32(cp) {
                            out.push(c);
                        }
                    }
                    i += 4;
                }
                c => out.push(c as char),
            }
            i += 1;
        } else {
            let l = utf8_len(b[i]);
            let end = (i + l).min(b.len());
            out.push_str(&s[i..end]);
            i = end;
        }
    }
    out
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

/// value 编码：转义 \n \t \r \\ 及其余控制字符，非 ASCII 按字面输出。
pub fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// key 编码：转义分隔符与行首敏感字符。
pub fn escape_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '=' | ':' | '#' | '!' | ' ' | '\\' => out.push_str(&format!("\\{c}")),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn parse(text: &str) -> LangFile {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" }.to_string();
    let mut lines = Vec::new();
    for raw_line in text.split_inclusive('\n') {
        let core = raw_line.trim_end_matches(['\r', '\n']);
        if core.starts_with('#') || core.starts_with('!') {
            lines.push(LangLine::Other(core.to_string()));
            continue;
        }
        // 找第一个未转义的 '='（或 ':'），Java 允许两者混用作分隔符。
        let b = core.as_bytes();
        let mut sep: Option<usize> = None;
        let mut i = 0usize;
        while i < b.len() {
            if b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i] == b'=' || b[i] == b':' {
                sep = Some(i);
                break;
            }
            i += 1;
        }
        match sep {
            Some(idx) => {
                // Java Properties 语义：分隔符两侧空白不计入键；值保留原文（含前导空白）
                let key_raw = core[..idx].trim_end_matches([' ', '\t']);
                let key = unescape(key_raw);
                let value = unescape(&core[idx + 1..]);
                lines.push(LangLine::Entry {
                    key,
                    value,
                    raw: core.to_string(),
                });
            }
            None => lines.push(LangLine::Other(core.to_string())),
        }
    }
    LangFile { lines, newline }
}

/// 按 updates 重写值；未变化的行保持原样输出（保留原始转义风格）。
pub fn rewrite(file: &LangFile, updates: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(256);
    for line in &file.lines {
        match line {
            LangLine::Other(raw) => {
                out.push_str(raw);
                out.push_str(&file.newline);
            }
            LangLine::Entry { key, value, raw } => match updates.get(key) {
                Some(new_v) if new_v != value => {
                    out.push_str(&escape_key(key));
                    out.push('=');
                    out.push_str(&escape_value(new_v));
                    out.push_str(&file.newline);
                }
                _ => {
                    out.push_str(raw);
                    out.push_str(&file.newline);
                }
            },
        }
    }
    out
}

/// 取出全部条目（保留重复键，Java 语义以最后一个为准，翻译时逐条同译）。
pub fn entries(file: &LangFile) -> Vec<(&str, &str)> {
    file.lines
        .iter()
        .filter_map(|l| match l {
            LangLine::Entry { key, value, .. } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_identity_rewrite() {
        let text = "# comment\r\nitem.dirt=泥土 Dirt\r\n\r\nitem.stone=Stone\\nLine2\r\nnoeq line\r\n";
        let f = parse(text);
        assert_eq!(f.newline, "\r\n");
        let out = rewrite(&f, &HashMap::new());
        assert_eq!(out, text);
        let e = entries(&f);
        assert_eq!(e[0], ("item.dirt", "泥土 Dirt"));
        assert_eq!(e[1], ("item.stone", "Stone\nLine2"));
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn rewrite_changes_only_target() {
        let text = "a=1\nb=2\n";
        let f = parse(text);
        let mut u = HashMap::new();
        u.insert("b".to_string(), "两个".to_string());
        let out = rewrite(&f, &u);
        assert_eq!(out, "a=1\nb=两个\n");
    }

    #[test]
    fn escapes_roundtrip() {
        let mut u = HashMap::new();
        u.insert("k".to_string(), "行1\n行2\t制表\\反斜杠=等号".to_string());
        let f = parse("k=x\n");
        let out = rewrite(&f, &u);
        let f2 = parse(&out);
        assert_eq!(entries(&f2)[0], ("k", "行1\n行2\t制表\\反斜杠=等号"));
    }

    #[test]
    fn colon_separator_and_escaped_eq() {
        let f = parse("a\\=b : c\nd:e\n");
        let e = entries(&f);
        assert_eq!(e[0], ("a=b", " c"));
        assert_eq!(e[1], ("d", "e"));
    }
}
