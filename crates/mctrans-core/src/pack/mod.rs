//! 整合包读写与目标文件识别。
//!
//! - zip/jar 全程流式：读旧包逐条目拷贝，命中规则的条目替换内容，写临时文件后原子改名；
//!   不解压到磁盘、不重打包（其它条目内容不变，压缩方式保持一致）；
//! - 支持 jar/zip 容器内部条目（`mods/x.jar!/assets/*/lang/en_us.json`）：
//!   容器内命中规则的条目会被翻译并整包重建，其余条目原样拷贝；
//! - 目录模式同样流式拷贝未命中文件；
//! - 目标规则用 glob 匹配（`**` 跨目录、`*` 单层；`!/` 在匹配时视作 `/`）。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::config::{FileKind, TargetRule};
use crate::error::{McError, McResult};
use crate::text::component;
use crate::text::encoding::{self, Bom, Decoded, Enc};
use crate::text::json_edit;
use crate::text::lang as langfile;
use crate::text::snbt;

/// 待译单元：文件内指针 + 解码后的字符串值。
#[derive(Debug, Clone)]
pub struct Unit {
    pub pointer: String,
    pub value: String,
    /// 值本身是否为 JSON 文本组件（需要二次展开）。
    pub comp: bool,
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub text: String,
    pub enc: Enc,
    pub bom: Bom,
    pub units: Vec<Unit>,
}

/// 解析目标文件，抽取待译单元。
pub fn analyze(kind: FileKind, text_keys: &[String], bytes: &[u8]) -> McResult<Analysis> {
    let dec: Decoded = encoding::decode(bytes, kind.utf8_only())?;
    let units = match kind {
        FileKind::Json => json_edit::parse_units(&dec.text, &|_| true)?
            .into_iter()
            .map(|u| Unit {
                pointer: u.pointer,
                value: u.value,
                comp: false,
            })
            .collect::<Vec<_>>(),
        FileKind::Lang => {
            let f = langfile::parse(&dec.text);
            let mut units = Vec::new();
            for line in &f.lines {
                if let langfile::LangLine::Entry { key, value, .. } = line {
                    units.push(Unit {
                        pointer: format!("key:{key}"),
                        value: value.clone(),
                        comp: false,
                    });
                }
            }
            units
        }
        FileKind::SnbtMap => snbt::parse_units(&dec.text, &snbt::SnbtMode::Map)?
            .into_iter()
            .map(|u| Unit {
                pointer: u.pointer,
                value: u.value,
                comp: false,
            })
            .collect(),
        FileKind::SnbtFields => snbt::parse_units(
            &dec.text,
            &snbt::SnbtMode::Fields(text_keys.to_vec()),
        )?
        .into_iter()
        .map(|u| Unit {
            pointer: u.pointer,
            value: u.value,
            comp: false,
        })
        .collect(),
    };
    let units = units
        .into_iter()
        .map(|mut u| {
            u.comp = component::is_component(&u.value);
            u
        })
        .collect();
    Ok(Analysis {
        text: dec.text,
        enc: dec.enc,
        bom: dec.bom,
        units,
    })
}

/// 把更新后的单元值写回原文（保形）。
pub fn apply_updates(
    kind: FileKind,
    text: &str,
    text_keys: &[String],
    updates: &[(String, String)],
) -> McResult<String> {
    match kind {
        FileKind::Json => json_edit::rewrite(text, updates, &|_| true),
        FileKind::Lang => {
            let f = langfile::parse(text);
            let mut map = HashMap::new();
            for (ptr, val) in updates {
                if let Some(k) = ptr.strip_prefix("key:") {
                    map.insert(k.to_string(), val.clone());
                }
            }
            Ok(langfile::rewrite(&f, &map))
        }
        FileKind::SnbtMap => snbt::rewrite(text, updates, &snbt::SnbtMode::Map),
        FileKind::SnbtFields => snbt::rewrite(text, updates, &snbt::SnbtMode::Fields(text_keys.to_vec())),
    }
}

/// zh_cn 模式下新语言文件的条目名：en_us → zh_cn。
pub fn translated_entry_name(rel: &str, target_lang: &str, kind: FileKind) -> String {
    if kind == FileKind::SnbtFields {
        return rel.to_string();
    }
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", rel),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (name, None),
    };
    let new_stem = if stem.eq_ignore_ascii_case("en_us") {
        target_lang.to_string()
    } else {
        format!("{stem}_{target_lang}")
    };
    let new_name = match ext {
        Some(e) => format!("{new_stem}.{e}"),
        None => new_stem,
    };
    if dir.is_empty() {
        new_name
    } else {
        format!("{dir}/{new_name}")
    }
}

/// glob 匹配：`**` 跨任意目录，`*` 单层内任意字符，`?` 单字符。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let ps: Vec<&str> = pattern.split('/').collect();
    let ts: Vec<&str> = text.split('/').collect();
    fn seg(p: &str, t: &str) -> bool {
        let pb: Vec<char> = p.chars().collect();
        let tb: Vec<char> = t.chars().collect();
        fn m(pb: &[char], tb: &[char]) -> bool {
            if pb.is_empty() {
                return tb.is_empty();
            }
            match pb[0] {
                '*' => m(&pb[1..], tb) || (!tb.is_empty() && m(pb, &tb[1..])),
                '?' => !tb.is_empty() && m(&pb[1..], &tb[1..]),
                c => !tb.is_empty() && tb[0] == c && m(&pb[1..], &tb[1..]),
            }
        }
        m(&pb, &tb)
    }
    fn go(ps: &[&str], ts: &[&str]) -> bool {
        match ps.first() {
            None => ts.is_empty(),
            Some(&"**") => {
                for skip in 0..=ts.len() {
                    if go(&ps[1..], &ts[skip..]) {
                        return true;
                    }
                }
                false
            }
            Some(p) => !ts.is_empty() && seg(p, ts[0]) && go(&ps[1..], &ts[1..]),
        }
    }
    go(&ps, &ts)
}

/// 匹配用规范化：jar 内部路径 `a.jar!/b` 视作 `a.jar/b`。
pub fn norm_for_match(path: &str) -> String {
    path.replace("!/", "/")
}

/// 找到文件命中的目标规则（纯 glob 匹配，按规则顺序取第一个）。
pub fn match_target(rules: &[TargetRule], rel: &str) -> Option<usize> {
    let lower = rel.to_lowercase();
    let normalized = norm_for_match(&lower);
    rules.iter().position(|r| {
        r.patterns
            .iter()
            .any(|p| glob_match(&p.to_lowercase(), &lower) || glob_match(&p.to_lowercase(), &normalized))
    })
}

#[derive(Debug, Clone)]
pub enum EntrySource {
    Fs(PathBuf),
    Zip { index: usize },
    /// jar/zip 容器内部条目。
    Nested {
        outer: Box<EntrySource>,
        inner: String,
    },
}

#[derive(Debug, Clone)]
pub struct PackFile {
    pub path: String,
    pub source: EntrySource,
    pub method: CompressionMethod,
}

/// 判断是否按 zip 容器处理（按扩展名）。
pub fn is_container_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".jar") || lower.ends_with(".zip")
}

fn is_zip_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes.starts_with(b"PK")
}

/// 枚举容器内部条目（跳过目录与不可读容器）。
fn inner_entries(bytes: &[u8]) -> McResult<Vec<(String, CompressionMethod)>> {
    let mut ar = ZipArchive::new(Cursor::new(bytes.to_vec()))
        .map_err(|e| McError::Other(format!("容器不是有效 zip: {e}")))?;
    let mut out = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let e = ar
            .by_index(i)
            .map_err(|e| McError::Other(format!("读取容器条目 {i} 失败: {e}")))?;
        if e.is_dir() {
            continue;
        }
        out.push((e.name().to_string(), e.compression()));
    }
    Ok(out)
}

enum PackKind {
    Zip,
    Dir(PathBuf),
}

pub struct PackReader {
    input: PathBuf,
    kind: PackKind,
    top: Vec<PackFile>,
}

impl PackReader {
    pub fn open(input: &Path) -> McResult<Self> {
        if !input.exists() {
            return Err(McError::Io {
                path: input.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "输入不存在"),
            });
        }
        if input.is_dir() {
            return Ok(Self {
                input: input.to_path_buf(),
                kind: PackKind::Dir(input.to_path_buf()),
                top: Vec::new(),
            });
        }
        let bytes = std::fs::read(input).map_err(|e| McError::Io {
            path: input.to_path_buf(),
            source: e,
        })?;
        if is_zip_bytes(&bytes) {
            let f = File::open(input).map_err(|e| McError::Io {
                path: input.to_path_buf(),
                source: e,
            })?;
            let mut ar = ZipArchive::new(f).map_err(|e| McError::Zip {
                path: input.to_path_buf(),
                source: e,
            })?;
            let mut top = Vec::with_capacity(ar.len());
            for i in 0..ar.len() {
                let entry = ar.by_index(i).map_err(|e| McError::Zip {
                    path: input.to_path_buf(),
                    source: e,
                })?;
                if entry.is_dir() {
                    continue;
                }
                top.push(PackFile {
                    path: entry.name().to_string(),
                    source: EntrySource::Zip { index: i },
                    method: entry.compression(),
                });
            }
            Ok(Self {
                input: input.to_path_buf(),
                kind: PackKind::Zip,
                top,
            })
        } else {
            Ok(Self {
                input: input.to_path_buf(),
                kind: PackKind::Dir(input.to_path_buf()),
                top: Vec::new(),
            })
        }
    }

    pub fn is_zip(&self) -> bool {
        matches!(self.kind, PackKind::Zip)
    }

    /// 顶层条目（不含 jar 内部）。
    pub fn top_files(&self) -> &[PackFile] {
        &self.top
    }

    /// 列出全部条目：顶层 + jar 内部（内部路径形如 `mods/x.jar!/assets/...`）。
    pub fn files(&self) -> McResult<Vec<PackFile>> {
        let mut out: Vec<PackFile> = Vec::new();
        match &self.kind {
            PackKind::Zip => {
                for (oi, f) in self.top.iter().enumerate() {
                    out.push(PackFile {
                        path: f.path.clone(),
                        source: match &f.source {
                            EntrySource::Zip { index } => EntrySource::Zip { index: *index },
                            _ => EntrySource::Fs(self.input.clone()),
                        },
                        method: f.method,
                    });
                    if !is_container_name(&f.path) {
                        continue;
                    }
                    let bytes = match self.read_top(oi) {
                        Ok(b) => b,
                        Err(_) => continue, // 容器损坏：按普通文件拷贝
                    };
                    if !is_zip_bytes(&bytes) {
                        continue;
                    }
                    let inners = match inner_entries(&bytes) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    for (name, method) in inners {
                        let full = format!("{}!/{}", f.path, name);
                        out.push(PackFile {
                            path: full,
                            source: EntrySource::Nested {
                                outer: Box::new(EntrySource::Zip { index: oi }),
                                inner: name,
                            },
                            method,
                        });
                    }
                }
            }
            PackKind::Dir(root) => {
                for e in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                    if e.file_type().is_file() {
                        let rel = e
                            .path()
                            .strip_prefix(root)
                            .unwrap_or(e.path())
                            .to_string_lossy()
                            .replace('\\', "/");
                        out.push(PackFile {
                            path: rel.clone(),
                            source: EntrySource::Fs(e.path().to_path_buf()),
                            method: CompressionMethod::Deflated,
                        });
                        if !is_container_name(&rel) {
                            continue;
                        }
                        let bytes = match std::fs::read(e.path()) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        if !is_zip_bytes(&bytes) {
                            continue;
                        }
                        let inners = match inner_entries(&bytes) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        for (name, method) in inners {
                            let full = format!("{rel}!/{name}");
                            out.push(PackFile {
                                path: full,
                                source: EntrySource::Nested {
                                    outer: Box::new(EntrySource::Fs(e.path().to_path_buf())),
                                    inner: name,
                                },
                                method,
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// 读顶层条目字节（zip 或 fs）。
    fn read_top(&self, oi: usize) -> McResult<Vec<u8>> {
        let f = self
            .top
            .get(oi)
            .ok_or_else(|| McError::Other("顶层条目下标越界".into()))?;
        match &f.source {
            EntrySource::Fs(p) => std::fs::read(p).map_err(|e| McError::Io {
                path: p.clone(),
                source: e,
            }),
            EntrySource::Zip { index } => {
                let file = File::open(&self.input).map_err(|e| McError::Io {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut ar = ZipArchive::new(file).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut entry = ar.by_index(*index).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).map_err(|e| McError::Io {
                    path: self.input.clone(),
                    source: e,
                })?;
                Ok(buf)
            }
            EntrySource::Nested { .. } => Err(McError::Other("read_top 不支持嵌套条目".into())),
        }
    }

    pub fn read(&self, file: &PackFile) -> McResult<Vec<u8>> {
        match &file.source {
            EntrySource::Fs(p) => std::fs::read(p).map_err(|e| McError::Io {
                path: p.clone(),
                source: e,
            }),
            EntrySource::Zip { index } => {
                let file_h = File::open(&self.input).map_err(|e| McError::Io {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut ar = ZipArchive::new(file_h).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut entry = ar.by_index(*index).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).map_err(|e| McError::Io {
                    path: self.input.clone(),
                    source: e,
                })?;
                Ok(buf)
            }
            EntrySource::Nested { outer, inner } => {
                let outer_bytes = match outer.as_ref() {
                    EntrySource::Fs(p) => std::fs::read(p).map_err(|e| McError::Io {
                        path: p.clone(),
                        source: e,
                    })?,
                    EntrySource::Zip { index } => {
                        let file = File::open(&self.input).map_err(|e| McError::Io {
                            path: self.input.clone(),
                            source: e,
                        })?;
                        let mut ar = ZipArchive::new(file).map_err(|e| McError::Zip {
                            path: self.input.clone(),
                            source: e,
                        })?;
                        let mut entry = ar.by_index(*index).map_err(|e| McError::Zip {
                            path: self.input.clone(),
                            source: e,
                        })?;
                        let mut buf = Vec::with_capacity(entry.size() as usize);
                        entry.read_to_end(&mut buf).map_err(|e| McError::Io {
                            path: self.input.clone(),
                            source: e,
                        })?;
                        buf
                    }
                    EntrySource::Nested { .. } => {
                        return Err(McError::Other("不支持多层嵌套容器".into()))
                    }
                };
                let mut ar = ZipArchive::new(Cursor::new(outer_bytes)).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut entry = ar.by_name(inner).map_err(|e| McError::Zip {
                    path: self.input.clone(),
                    source: e,
                })?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).map_err(|e| McError::Io {
                    path: self.input.clone(),
                    source: e,
                })?;
                Ok(buf)
            }
        }
    }
}

/// 重建 zip/jar 容器：replacements 替换同名条目，additions 追加新条目，其余原样拷贝。
pub fn rewrite_zip_container(
    outer: &[u8],
    replacements: &HashMap<String, Vec<u8>>,
    additions: &[(String, Vec<u8>)],
) -> McResult<Vec<u8>> {
    let mut reader = ZipArchive::new(Cursor::new(outer.to_vec()))
        .map_err(|e| McError::Other(format!("容器不是有效 zip: {e}")))?;
    let mut out_buf: Vec<u8> = Vec::new();
    {
        let mut writer = ZipWriter::new(Cursor::new(&mut out_buf));
        for i in 0..reader.len() {
            let mut e = reader
                .by_index(i)
                .map_err(|e| McError::Other(format!("读取容器条目失败: {e}")))?;
            let name = e.name().to_string();
            let opts = SimpleFileOptions::default()
                .compression_method(e.compression())
                .large_file(e.size() > u32::MAX as u64);
            if let Some(new_bytes) = replacements.get(&name) {
                if e.is_dir() {
                    continue;
                }
                writer
                    .start_file(name.clone(), opts)
                    .map_err(|e| McError::Other(format!("写入容器条目 {name} 失败: {e}")))?;
                writer.write_all(new_bytes).map_err(|e| McError::Io {
                    path: PathBuf::from("<container>"),
                    source: e,
                })?;
            } else {
                writer
                    .start_file(name.clone(), opts)
                    .map_err(|e| McError::Other(format!("拷贝容器条目 {name} 失败: {e}")))?;
                std::io::copy(&mut e, &mut writer).map_err(|e| McError::Io {
                    path: PathBuf::from("<container>"),
                    source: e,
                })?;
            }
        }
        for (name, bytes) in additions {
            let opts = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .large_file(bytes.len() as u64 > u32::MAX as u64);
            writer
                .start_file(name.clone(), opts)
                .map_err(|e| McError::Other(format!("新增容器条目 {name} 失败: {e}")))?;
            writer.write_all(bytes).map_err(|e| McError::Io {
                path: PathBuf::from("<container>"),
                source: e,
            })?;
        }
        writer
            .finish()
            .map_err(|e| McError::Other(format!("关闭容器失败: {e}")))?;
    }
    Ok(out_buf)
}

/// 输出包写入器：zip 写临时文件、目录直接落盘，finish 时原子改名。
pub struct PackWriter {
    output: PathBuf,
    temp: PathBuf,
    zip: Option<ZipWriter<File>>,
    dir_root: Option<PathBuf>,
    lock: PathBuf,
}

impl PackWriter {
    pub fn create(output: &Path, is_zip: bool) -> McResult<Self> {
        let lock = output.with_extension("mctrans.lock");
        if lock.exists() {
            return Err(McError::OutputLocked { path: lock });
        }
        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| McError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
        }
        std::fs::write(&lock, "").map_err(|e| McError::Io {
            path: lock.clone(),
            source: e,
        })?;
        let temp = output.with_extension("mctrans.tmp");
        if is_zip {
            let f = File::create(&temp).map_err(|e| McError::Io {
                path: temp.clone(),
                source: e,
            })?;
            Ok(Self {
                output: output.to_path_buf(),
                temp,
                zip: Some(ZipWriter::new(f)),
                dir_root: None,
                lock,
            })
        } else {
            std::fs::create_dir_all(output).map_err(|e| McError::Io {
                path: output.to_path_buf(),
                source: e,
            })?;
            Ok(Self {
                output: output.to_path_buf(),
                temp: output.to_path_buf(),
                zip: None,
                dir_root: Some(output.to_path_buf()),
                lock,
            })
        }
    }

    pub fn write(&mut self, rel: &str, bytes: &[u8], method: CompressionMethod) -> McResult<()> {
        match &mut self.zip {
            Some(w) => {
                let opts = SimpleFileOptions::default()
                    .compression_method(method)
                    .large_file(bytes.len() as u64 > u32::MAX as u64);
                w.start_file(rel, opts).map_err(|e| McError::Other(format!(
                    "写入 zip 条目 {rel} 失败: {e}"
                )))?;
                w.write_all(bytes).map_err(|e| McError::Io {
                    path: self.temp.clone(),
                    source: e,
                })
            }
            None => {
                let root = self.dir_root.clone().unwrap();
                let target = root.join(rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| McError::Io {
                        path: parent.to_path_buf(),
                        source: e,
                    })?;
                }
                std::fs::write(&target, bytes).map_err(|e| McError::Io {
                    path: target,
                    source: e,
                })
            }
        }
    }

    /// 原样拷贝条目（未命中规则的条目）。
    pub fn copy_entry(&mut self, file: &PackFile, bytes: Vec<u8>) -> McResult<()> {
        let method = match self.zip {
            Some(_) => file.method,
            None => CompressionMethod::Stored,
        };
        self.write(&file.path, &bytes, method)
    }

    pub fn finish(mut self) -> McResult<PathBuf> {
        let output = self.output.clone();
        if let Some(w) = self.zip.take() {
            w.finish().map_err(|e| McError::Other(format!("关闭 zip 失败: {e}")))?;
            std::fs::rename(&self.temp, &self.output).map_err(|e| McError::Io {
                path: self.temp.clone(),
                source: e,
            })?;
        }
        let _ = std::fs::remove_file(&self.lock);
        Ok(output)
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
        if self.zip.is_some() {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// 默认输出路径：MyPack.zip → MyPack-zh_cn.zip；目录 → MyPack-zh_cn。
pub fn default_output(input: &Path, target_lang: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "pack".into());
    if input.is_dir() {
        parent.join(format!("{stem}-{target_lang}"))
    } else {
        let ext = input
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "zip".into());
        parent.join(format!("{stem}-{target_lang}.{ext}"))
    }
}

/// 嵌套条目集合（容器名 → 内部条目）。
pub type NestedMap = HashMap<String, Vec<(String, CompressionMethod)>>;

/// 从 files() 结果中拆分顶层与嵌套条目。
pub fn split_nested(files: &[PackFile]) -> (Vec<PackFile>, NestedMap) {
    let mut tops = Vec::new();
    let mut nested: NestedMap = HashMap::new();
    for f in files {
        match &f.source {
            EntrySource::Nested { inner, .. } => {
                if let Some((outer, _)) = f.path.split_once("!/") {
                    nested
                        .entry(outer.to_string())
                        .or_default()
                        .push((inner.clone(), f.method));
                }
            }
            _ => tops.push(PackFile {
                path: f.path.clone(),
                source: match &f.source {
                    EntrySource::Fs(p) => EntrySource::Fs(p.clone()),
                    EntrySource::Zip { index } => EntrySource::Zip { index: *index },
                    EntrySource::Nested { .. } => unreachable!(),
                },
                method: f.method,
            }),
        }
    }
    (tops, nested)
}

/// 全量条目名集合（含嵌套路径）。
pub fn all_names(files: &[PackFile]) -> HashSet<String> {
    files.iter().map(|f| f.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match(
            "mods/**/assets/*/lang/en_us.json",
            "mods/fancy-1.20/assets/mymod/lang/en_us.json"
        ));
        assert!(!glob_match("mods/*/lang/en_us.json", "mods/a/b/lang/en_us.json"));
        assert!(glob_match("config/ftbquests/**/*.snbt", "config/ftbquests/quests/chapters/abc.snbt"));
        assert!(glob_match("**/*.json", "a/b/c.json"));
        assert!(!glob_match("**/a.json", "a.txt"));
        assert!(glob_match("a?c", "abc"));
    }

    #[test]
    fn container_path_matching() {
        let rules = crate::config::default_targets();
        let idx = match_target(&rules, "mods/jei-1.20.jar!/assets/jei/lang/en_us.json").unwrap();
        assert_eq!(rules[idx].name, "mod-lang-json");
        let idx = match_target(&rules, "mods/old.jar!/assets/old/lang/en_us.lang").unwrap();
        assert_eq!(rules[idx].name, "mod-lang-legacy");
        assert!(match_target(&rules, "mods/plain.jar").is_none());
    }

    #[test]
    fn entry_name_translation() {
        assert_eq!(
            translated_entry_name("assets/m/lang/en_us.json", "zh_cn", FileKind::Json),
            "assets/m/lang/zh_cn.json"
        );
        assert_eq!(
            translated_entry_name("config/ftbquests/quests/lang/en_us.snbt", "zh_cn", FileKind::SnbtMap),
            "config/ftbquests/quests/lang/zh_cn.snbt"
        );
        assert_eq!(
            translated_entry_name("config/ftbquests/quests/chapters/a.snbt", "zh_cn", FileKind::SnbtFields),
            "config/ftbquests/quests/chapters/a.snbt"
        );
        assert_eq!(
            translated_entry_name("assets/m/lang/de_de.json", "zh_cn", FileKind::Json),
            "assets/m/lang/de_de_zh_cn.json"
        );
    }

    #[test]
    fn container_rebuild_preserves_other_entries() {
        // 构造容器
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"Manifest-Version: 1.0\n").unwrap();
            w.start_file("assets/jei/lang/en_us.json", opts).unwrap();
            w.write_all(br#"{"a":"A"}"#).unwrap();
            w.start_file("assets/jei/textures/x.png", opts).unwrap();
            w.write_all(&[0x89, 0x50, 0x4E, 0x47, 1, 2, 3]).unwrap();
            w.finish().unwrap();
        }
        let mut repl = HashMap::new();
        let translated = "{\"a\":\"\u{8bd1}\"}".as_bytes().to_vec();
        repl.insert("assets/jei/lang/en_us.json".to_string(), translated.clone());
        let out = rewrite_zip_container(&buf, &repl, &[("assets/jei/lang/zh_cn.json".to_string(), translated)]).unwrap();
        let mut r = ZipArchive::new(Cursor::new(out)).unwrap();
        let names: Vec<String> = (0..r.len()).map(|i| r.by_index(i).unwrap().name().to_string()).collect();
        assert!(names.contains(&"META-INF/MANIFEST.MF".to_string()));
        assert!(names.contains(&"assets/jei/textures/x.png".to_string()));
        assert!(names.contains(&"assets/jei/lang/zh_cn.json".to_string()));
        assert_eq!(names.len(), 4);
        let mut e = r.by_name("assets/jei/lang/en_us.json").unwrap();
        let mut s = String::new();
        e.read_to_string(&mut s).unwrap();
        assert_eq!(s, "{\"a\":\"\u{8bd1}\"}");
        drop(e);
        let mut e = r.by_name("assets/jei/textures/x.png").unwrap();
        let mut b = Vec::new();
        e.read_to_end(&mut b).unwrap();
        assert_eq!(b, vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3]);
    }

    #[test]
    fn analyze_json_and_rewrite() {
        let bytes = br#"{"tile.dirt.name":"Dirt","tile.dirt.desc":"A %s block"}"#;
        let a = analyze(FileKind::Json, &[], bytes).unwrap();
        assert_eq!(a.units.len(), 2);
        assert_eq!(a.units[0].value, "Dirt");
        let updates = vec![("/tile.dirt.name".into(), "泥土".into())];
        let out = apply_updates(FileKind::Json, &a.text, &[], &updates).unwrap();
        assert_eq!(out, "{\"tile.dirt.name\":\"泥土\",\"tile.dirt.desc\":\"A %s block\"}");
    }

    #[test]
    fn analyze_lang_and_rewrite() {
        let bytes = b"item.dirt=Dirt\n# comment\nitem.stone=Stone\n";
        let a = analyze(FileKind::Lang, &[], bytes).unwrap();
        assert_eq!(a.units.len(), 2);
        let updates = vec![("key:item.dirt".into(), "泥土".into())];
        let out = apply_updates(FileKind::Lang, &a.text, &[], &updates).unwrap();
        assert_eq!(out, "item.dirt=泥土\n# comment\nitem.stone=Stone\n");
    }

    #[test]
    fn analyze_snbt_fields_with_component() {
        let bytes = br#"{title: "Chapter 1", description: '{"text":"Get wood","color":"gold"}'}"#;
        let a = analyze(
            FileKind::SnbtFields,
            &["title".to_string(), "description".to_string()],
            bytes,
        )
        .unwrap();
        assert_eq!(a.units.len(), 2);
        assert!(!a.units[0].comp); // "Chapter 1" 是普通字符串
        assert!(a.units[1].comp); // 值是 JSON 文本组件
    }

    #[test]
    fn split_nested_paths() {
        // 目录模式下构造假 files 列表验证拆分
        let files = vec![
            PackFile {
                path: "mods/a.jar".into(),
                source: EntrySource::Fs(PathBuf::from("mods/a.jar")),
                method: CompressionMethod::Deflated,
            },
            PackFile {
                path: "mods/a.jar!/assets/m/lang/en_us.json".into(),
                source: EntrySource::Nested {
                    outer: Box::new(EntrySource::Fs(PathBuf::from("mods/a.jar"))),
                    inner: "assets/m/lang/en_us.json".into(),
                },
                method: CompressionMethod::Deflated,
            },
            PackFile {
                path: "config/x.json".into(),
                source: EntrySource::Fs(PathBuf::from("config/x.json")),
                method: CompressionMethod::Stored,
            },
        ];
        let (tops, nested) = split_nested(&files);
        assert_eq!(tops.len(), 2);
        assert_eq!(nested.len(), 1);
        assert_eq!(nested["mods/a.jar"][0].0, "assets/m/lang/en_us.json");
    }
}
