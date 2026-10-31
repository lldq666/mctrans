//! JSON 结构编辑器：只改字符串 value 的字符区间，缩进、键顺序、尾逗号、其它字符串全部逐字节保留。
//! en_us.json 与 FTB Quests 内嵌 JSON 文本组件共用。

use std::collections::HashMap;

use crate::error::{io_err, McError, McResult};

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// JSON Pointer，如 `/tile.dirt.name`、`/extra/0/text`。
    pub pointer: String,
    /// 解码后的字符串值。
    pub value: String,
}

struct Span {
    pointer: String,
    /// value 内容起止（不含引号）。
    start: usize,
    end: usize,
    /// 原引号字符（JSON 只允许双引号，预留）。
    #[allow(dead_code)]
    quote: u8,
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn err(&self, msg: &str) -> McError {
        McError::Decode {
            path: "json".into(),
            msg: format!("偏移 {}: {msg}", self.i),
        }
    }

    fn scan(&mut self, accept: &dyn Fn(&str) -> bool, out: &mut Vec<Span>) -> McResult<()> {
        self.ws();
        match self.peek() {
            None => Ok(()),
            _ => self.value(String::new(), accept, out),
        }
    }

    fn value(&mut self, ptr: String, accept: &dyn Fn(&str) -> bool, out: &mut Vec<Span>) -> McResult<()> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(ptr, accept, out),
            Some(b'[') => self.array(ptr, accept, out),
            Some(b'"') => {
                let s = self.string()?;
                if accept(&ptr) {
                    out.push(Span {
                        pointer: ptr,
                        start: s.0,
                        end: s.1,
                        quote: b'"',
                    });
                }
                Ok(())
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(b't') => self.lit("true"),
            Some(b'f') => self.lit("false"),
            Some(b'n') => self.lit("null"),
            _ => Err(self.err("不是合法的 JSON 值")),
        }
    }

    fn number(&mut self) -> McResult<()> {
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn lit(&mut self, word: &str) -> McResult<()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(())
        } else {
            Err(self.err(&format!("不是 {word}")))
        }
    }

    fn object(&mut self, ptr: String, accept: &dyn Fn(&str) -> bool, out: &mut Vec<Span>) -> McResult<()> {
        self.i += 1; // {
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("对象键必须是字符串"));
            }
            let (ks, ke) = self.string_raw()?;
            let raw = &std::str::from_utf8(&self.b[ks..ke]).map_err(|_| self.err("键不是 UTF-8"))?;
            let key = decode_json_string(raw)?;
            let child = format!("{ptr}/{}", escape_pointer(&key));
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("缺少 ':'"));
            }
            self.i += 1;
            self.value(child, accept, out)?;
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.err("对象缺少 ',' 或 '}'")),
            }
        }
    }

    fn array(&mut self, ptr: String, accept: &dyn Fn(&str) -> bool, out: &mut Vec<Span>) -> McResult<()> {
        self.i += 1; // [
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        let mut idx = 0usize;
        loop {
            let child = format!("{ptr}/{idx}");
            self.value(child, accept, out)?;
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    idx += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.err("数组缺少 ',' 或 ']'")),
            }
        }
    }

    /// 扫描一个字符串字面量，返回 (内容起, 内容止)（不含首尾引号）。
    fn string_raw(&mut self) -> McResult<(usize, usize)> {
        let quote = self.peek().ok_or_else(|| self.err("意外的文件结尾"))?;
        let start = self.i + 1;
        self.i += 1;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => {
                    self.i += 2;
                }
                c if c == quote => {
                    let end = self.i;
                    self.i += 1;
                    return Ok((start, end));
                }
                _ => self.i += 1,
            }
        }
        Err(self.err("字符串未闭合"))
    }

    /// 扫描字符串字面量并解码其内容。
    fn string(&mut self) -> McResult<(usize, usize)> {
        self.string_raw()
    }
}

/// 解析 JSON，取出满足 accept(pointer) 的全部字符串单元。
pub fn parse_units(src: &str, accept: &dyn Fn(&str) -> bool) -> McResult<Vec<Unit>> {
    let mut sc = Scanner::new(src);
    let mut spans = Vec::new();
    sc.scan(accept, &mut spans)?;
    sc.ws();
    if sc.i != src.len() {
        return Err(McError::Decode {
            path: "json".into(),
            msg: format!("根值之后还有多余内容（偏移 {}）", sc.i),
        });
    }
    let mut units = Vec::with_capacity(spans.len());
    for s in spans {
        let raw = src
            .get(s.start..s.end)
            .ok_or_else(|| McError::Decode {
                path: "json".into(),
                msg: "字符串区间越界".into(),
            })?;
        units.push(Unit {
            pointer: s.pointer,
            value: decode_json_string(raw)?,
        });
    }
    Ok(units)
}

/// 按 updates 重写指定 pointer 的字符串值，其余部分逐字节保留。
pub fn rewrite(src: &str, updates: &[(String, String)], accept: &dyn Fn(&str) -> bool) -> McResult<String> {
    if updates.is_empty() {
        return Ok(src.to_string());
    }
    let mut sc = Scanner::new(src);
    let mut spans: Vec<Span> = Vec::new();
    sc.scan(&|_| true, &mut spans)?;
    sc.ws();
    if sc.i != src.len() {
        return Err(McError::Decode {
            path: "json".into(),
            msg: format!("根值之后还有多余内容（偏移 {}）", sc.i),
        });
    }
    let map: HashMap<&str, &str> = updates.iter().map(|(p, v)| (p.as_str(), v.as_str())).collect();
    let mut out = String::with_capacity(src.len() + 64);
    let mut last = 0usize;
    let mut applied = HashSet::new();
    for s in &spans {
        if !accept(&s.pointer) {
            continue;
        }
        let new_v = match map.get(s.pointer.as_str()) {
            Some(v) => *v,
            None => continue,
        };
        out.push_str(&src[last..s.start]);
        out.push_str(&escape_content(new_v));
        last = s.end;
        applied.insert(s.pointer.clone());
    }
    out.push_str(&src[last..]);
    if applied.len() != updates.len() {
        let missing: Vec<&String> = updates
            .iter()
            .filter(|(p, _)| !applied.contains(p))
            .map(|(p, _)| p)
            .collect();
        return Err(McError::Other(format!("JSON 重写时找不到目标指针: {missing:?}")));
    }
    // 安全网：重写后必须仍是合法 JSON。
    parse_units(&out, &|_| true)?;
    Ok(out)
}

use std::collections::HashSet;

/// 解码 JSON 字符串内容（已去掉首尾引号）。
pub fn decode_json_string(raw: &str) -> McResult<String> {
    let b = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 1;
                if i >= b.len() {
                    return Err(dec_err("字符串转义不完整"));
                }
                match b[i] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let cp = read_hex4(b, i + 1).ok_or_else(|| dec_err("\\u 转义非法"))?;
                        i += 4;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            // 高位代理，尝试读取低位代理
                            if b.get(i + 1) == Some(&b'\\') && b.get(i + 2) == Some(&b'u') {
                                let lo = read_hex4(b, i + 3).ok_or_else(|| dec_err("代理对转义非法"))?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).ok_or_else(|| dec_err("代理对非法"))?);
                                    i += 6; // 跳过低位代理的 \uXXXX（末尾统一 +1）
                                } else {
                                    return Err(dec_err("孤立低位代理"));
                                }
                            } else {
                                return Err(dec_err("孤立高位代理"));
                            }
                        } else {
                            out.push(char::from_u32(cp).ok_or_else(|| dec_err("\\u 码点非法"))?);
                        }
                    }
                    _ => return Err(dec_err("未知转义")),
                }
                i += 1;
            }
            _ => {
                // 原样拷贝一个 UTF-8 字符
                let l = utf8_len(b[i]);
                let end = (i + l).min(b.len());
                let ch = std::str::from_utf8(&b[i..end]).map_err(|_| dec_err("非 UTF-8 字符"))?;
                out.push_str(ch);
                i = end;
            }
        }
    }
    Ok(out)
}

fn dec_err(msg: &str) -> McError {
    McError::Decode {
        path: "json".into(),
        msg: msg.to_string(),
    }
}

fn read_hex4(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    let mut v = 0u32;
    for &c in s {
        let d = (c as char).to_digit(16)?;
        v = v * 16 + d;
    }
    Some(v)
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

/// 转义字符串内容（不含首尾引号）。
pub fn escape_content(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 把字符串编码为 JSON value 字面量（含首尾引号）。
pub fn escape_json_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 10);
    out.push('"');
    out.push_str(&escape_content(s));
    out.push('"');
    out
}

/// JSON Pointer 段转义（~ → ~0，/ → ~1）。
pub fn escape_pointer(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// 从 pointer 中取出最后一段（未转义还原）。
pub fn last_segment(pointer: &str) -> String {
    let seg = pointer.rsplit('/').next().unwrap_or(pointer);
    seg.replace("~1", "/").replace("~0", "~")
}

/// 文件级读写辅助：读文件失败时带路径。
pub fn read_to_string(path: &std::path::Path) -> McResult<String> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    String::from_utf8(bytes).map_err(|_| McError::Decode {
        path: path.display().to_string(),
        msg: "不是 UTF-8 文件".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_all(_: &str) -> bool {
        true
    }

    #[test]
    fn roundtrip_identity() {
        let src = r#"{
  "tile.dirt.name": "Dirt",
  "quote": "He said \"hi\"\nto %s",
  "nested": {"a": [1, 2, {"b": "c"}], "d": null},
  "trailing": {
  }
}"#;
        let out = rewrite(src, &[], &accept_all).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn replace_value_keeps_layout() {
        let src = "{\n  \"a\": \"Dirt\",\n  \"b\": \"拾取 %s 个泥土\"\n}";
        let out = rewrite(src, &[("/a".into(), "泥土".into())], &accept_all).unwrap();
        assert_eq!(out, "{\n  \"a\": \"泥土\",\n  \"b\": \"拾取 %s 个泥土\"\n}");
        let units = parse_units(&out, &accept_all).unwrap();
        assert_eq!(units[0].value, "泥土");
    }

    #[test]
    fn reescape_newlines() {
        let src = "{\"a\": \"x\"}";
        let out = rewrite(src, &[("/a".into(), "line1\nline2\t\"q\"".into())], &accept_all).unwrap();
        assert_eq!(out, "{\"a\": \"line1\\nline2\\t\\\"q\\\"\"}");
        let units = parse_units(&out, &accept_all).unwrap();
        assert_eq!(units[0].value, "line1\nline2\t\"q\"");
    }

    #[test]
    fn pointers_and_last_segment() {
        assert_eq!(last_segment("/extra/0/text"), "text");
        assert_eq!(last_segment("/a~1b"), "a/b");
        assert_eq!(escape_pointer("a/b"), "a~1b");
    }

    #[test]
    fn invalid_json_rejected() {
        assert!(parse_units("{", &accept_all).is_err());
        assert!(parse_units("{\"a\": 1,}", &accept_all).is_err());
        assert!(parse_units("{} x", &accept_all).is_err());
    }

    #[test]
    fn reject_missing_pointer() {
        let src = "{\"a\": \"x\"}";
        assert!(rewrite(src, &[("/b".into(), "y".into())], &accept_all).is_err());
    }

    #[test]
    fn surrogate_pair_decode() {
        let units = parse_units(r#"{"a":"\ud83d\ude00"}"#, &accept_all).unwrap();
        assert_eq!(units[0].value, "😀");
    }
}
