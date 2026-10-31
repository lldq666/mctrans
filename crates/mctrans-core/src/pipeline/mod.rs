//! 编排管线：扫描 → 解析 → 掩码 → 缓存/渠道 → 校验修复 → 保形写回 → 报告。
//!
//! 关键行为：
//! - 逐文件处理、逐文件提交（zip 流式拷贝，失败文件原样拷贝并记录，不中断整体）；
//! - jar/zip 容器：内部命中条目整包重建，其余条目原样拷贝；
//! - 批次级并发（信号量控制），渠道链在批次内部重试/回退/熔断；
//! - 占位符校验失败的译文被丢弃，保留原文并写入报告；
//! - --resume 时跳过上次已完成的文件（缓存仍生效，重复成本接近零）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::cache::Cache;
use crate::config::{self, Config, FileKind, TranslateMode};
use crate::error::{McError, McResult};
use crate::pack::{
    self, all_names, apply_updates, analyze, is_container_name, match_target, rewrite_zip_container,
    split_nested, translated_entry_name, PackFile, PackReader, PackWriter,
};
use crate::placeholder::{has_cjk, looks_translatable, PlaceholderEngine};
use crate::providers::{Breakers, ChainRunner, IntervalLimiter, ReqCtx, Translator};
use crate::report::{FailureEntry, ProviderUsage, RunReport};
use crate::text::component;
use crate::text::encoding;

pub struct TranslateOpts {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub dry_run: bool,
    pub force: bool,
    pub no_cache: bool,
    pub resume: bool,
    pub concurrency: usize,
    pub provider_override: Option<String>,
    pub config_path: Option<PathBuf>,
    pub system_suffix: Option<String>,
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub on_event: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanReport {
    pub pack: String,
    pub files_matched: usize,
    pub units_total: usize,
    pub chars_total: usize,
    pub cache_hits: usize,
    pub to_translate: usize,
}

fn emit(opts: &TranslateOpts, msg: impl Into<String>) {
    if let Some(cb) = &opts.on_event {
        cb(msg.into());
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// 单个待译任务：外层单元指针 + 可选的组件内指针。
struct Task {
    unit: usize,
    comp: Option<String>,
    source: String,
}

/// 文件处理计划：普通文件或 jar 容器。
enum Plan {
    Plain {
        file: PackFile,
        rule: usize,
    },
    Jar {
        outer: PackFile,
        /// (容器内路径, 规则下标)
        inners: Vec<(String, usize)>,
        bytes: Vec<u8>,
    },
}

#[derive(Default)]
struct Counters {
    cache_hits: AtomicUsize,
    translated: AtomicUsize,
    placeholder_failures: AtomicUsize,
    failed: AtomicUsize,
}

/// 单文件处理所需的共享上下文。
struct RunCtx<'a> {
    cfg: &'a Config,
    engine: &'a Arc<PlaceholderEngine>,
    cache: &'a Option<Arc<Cache>>,
    runner: &'a Arc<ChainRunner>,
    breakers: &'a Arc<Breakers>,
    semaphore: &'a Arc<Semaphore>,
    glossary: &'a Arc<Vec<(String, String)>>,
    req_ctx: &'a Arc<ReqCtx>,
    counters: &'a Arc<Counters>,
    usage: &'a Arc<Mutex<HashMap<String, ProviderUsage>>>,
    active_provider: String,
    dry_run: bool,
    force: bool,
}

pub async fn translate(cfg: &Config, opts: &TranslateOpts) -> McResult<RunReport> {
    let started = Instant::now();
    cfg.validate()?;
    let order = cfg.provider_order(opts.provider_override.as_deref());
    if order.is_empty() {
        return Err(McError::Config(
            "没有配置任何翻译渠道：请参考 README 编写 mctrans.toml 的 [providers] 段".into(),
        ));
    }

    let reader = PackReader::open(&opts.input)?;
    let is_zip = reader.is_zip();
    let out_path = opts
        .output
        .clone()
        .unwrap_or_else(|| pack::default_output(&opts.input, &cfg.general.target_lang));
    let mut report = RunReport::default();
    report.pack = opts.input.display().to_string();
    report.output = out_path.display().to_string();
    report.src_lang = cfg.general.src_lang.clone();
    report.tgt_lang = cfg.general.target_lang.clone();
    report.mode = match cfg.general.mode {
        TranslateMode::ZhCn => "zh_cn".into(),
        TranslateMode::Inplace => "inplace".into(),
    };
    report.dry_run = opts.dry_run;
    report.provider_order = order.clone();
    report.active_provider = order.first().cloned().unwrap_or_default();

    // ---- 渠道实例 ----
    let mut instances: Vec<(String, Arc<dyn Translator>)> = Vec::new();
    let mut limiters: HashMap<String, Arc<IntervalLimiter>> = HashMap::new();
    let mut missing = Vec::new();
    if !opts.dry_run {
        for name in &order {
            let pcfg = &cfg.providers[name];
            match crate::providers::build_provider(name, pcfg, cfg.resolve_api_key(name)) {
                Ok(p) => {
                    limiters.insert(name.clone(), Arc::new(IntervalLimiter::new(Duration::from_millis(150))));
                    instances.push((name.clone(), p));
                }
                Err(e) => missing.push(format!("渠道 {name} 不可用：{e}")),
            }
        }
        if instances.is_empty() {
            return Err(McError::Config(format!(
                "没有可用的翻译渠道。{}",
                missing.join("；")
            )));
        }
        for m in &missing {
            tracing::warn!("{m}");
        }
    }
    let runner = Arc::new(ChainRunner {
        providers: instances,
        max_retries: cfg.limits.max_retries,
        base_delay: Duration::from_millis(400),
        breaker_threshold: cfg.fallback.circuit_breaker_failures.max(1),
        limiters,
    });
    let breakers = Arc::new(Breakers::default());
    let engine = Arc::new(PlaceholderEngine::new(&cfg.placeholder)?);

    // ---- 缓存 ----
    let use_cache = cfg.general.cache && !opts.no_cache;
    let cache = if use_cache {
        Some(Arc::new(Cache::open(&config::resolve_db_path(cfg))?))
    } else {
        None
    };
    let run_id = match (&cache, opts.dry_run) {
        (Some(c), false) => Some(c.new_run(&cfg.general.target_lang, &report.mode, &report.pack)?),
        _ => None,
    };
    let glossary = Arc::new(match &cache {
        Some(c) => c.glossary_list().unwrap_or_default(),
        None => Vec::new(),
    });
    let req_ctx = Arc::new(ReqCtx {
        src_lang: cfg.general.src_lang.clone(),
        tgt_lang: cfg.general.target_lang.clone(),
        system_suffix: opts.system_suffix.clone(),
        glossary: glossary.iter().take(30).cloned().collect(),
    });

    // ---- 输出保护 ----
    if !opts.dry_run {
        if same_path(&opts.input, &out_path) && cfg.general.mode != TranslateMode::Inplace && !opts.force {
            return Err(McError::Config(format!(
                "输出路径与输入相同（{}）。inplace 模式或 --force 才允许覆盖原包",
                out_path.display()
            )));
        }
        if out_path.exists() && !opts.force {
            return Err(McError::Config(format!(
                "输出已存在：{}（加 --force 覆盖，或换一个 -o 路径）",
                out_path.display()
            )));
        }
    }

    // ---- 枚举与计划 ----
    let files = reader.files()?;
    report.files_total = files.len();
    let names: HashSet<String> = all_names(&files);
    let mut resume_done: HashSet<String> = HashSet::new();
    if opts.resume {
        if let Some(c) = &cache {
            resume_done = c.latest_run_done_files(&report.pack)?;
            if !resume_done.is_empty() {
                emit(opts, format!("resume：跳过 {} 个已完成文件", resume_done.len()));
            }
        }
    }

    let (tops, nested) = split_nested(&files);
    let mut plans: Vec<Plan> = Vec::new();
    let mut plain_copies: Vec<PackFile> = Vec::new();
    for file in &tops {
        if is_container_name(&file.path) {
            let bytes = match reader.read(file) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("读取容器 {} 失败：{e}，按普通文件拷贝", file.path);
                    plain_copies.push(file.clone());
                    continue;
                }
            };
            let inners = nested
                .get(&file.path)
                .map(|v| {
                    v.iter()
                        .filter_map(|(name, _)| {
                            // 用完整容器路径匹配规则：mods/x.jar!/assets/...
                            let full = format!("{}!/{}", file.path, name);
                            match_target(&cfg.targets, &full).map(|r| (name.clone(), r))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if inners.is_empty() {
                plain_copies.push(file.clone());
            } else {
                plans.push(Plan::Jar {
                    outer: file.clone(),
                    inners,
                    bytes,
                });
            }
            continue;
        }
        match match_target(&cfg.targets, &file.path) {
            Some(rule) => plans.push(Plan::Plain {
                file: file.clone(),
                rule,
            }),
            None => plain_copies.push(file.clone()),
        }
    }

    let mut writer = if opts.dry_run {
        None
    } else {
        Some(PackWriter::create(&out_path, is_zip)?)
    };

    let semaphore = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let counters = Arc::new(Counters::default());
    let usage: Arc<Mutex<HashMap<String, ProviderUsage>>> = Arc::new(Mutex::new(HashMap::new()));
    let rctx = RunCtx {
        cfg,
        engine: &engine,
        cache: &cache,
        runner: &runner,
        breakers: &breakers,
        semaphore: &semaphore,
        glossary: &glossary,
        req_ctx: &req_ctx,
        counters: &counters,
        usage: &usage,
        active_provider: report.active_provider.clone(),
        dry_run: opts.dry_run,
        force: opts.force,
    };

    // resume 跳过的文件原样拷贝
    for file in &plain_copies {
        if resume_done.contains(&file.path) {
            report.files_skipped += 1;
        }
        if let Some(w) = writer.as_mut() {
            let bytes = reader.read(file)?;
            w.copy_entry(file, bytes)?;
        }
    }

    'plans: for plan in &plans {
        if let Some(c) = &opts.cancel {
            if c.load(Ordering::Relaxed) {
                emit(opts, "收到取消信号，停止处理（已完成文件保留）");
                report.failures.push(FailureEntry {
                    container: "*".into(),
                    unit: "*".into(),
                    source: String::new(),
                    reason: "用户取消".into(),
                });
                break 'plans;
            }
        }
        match plan {
            Plan::Plain { file, rule } => {
                if resume_done.contains(&file.path) {
                    report.files_skipped += 1;
                    if let Some(w) = writer.as_mut() {
                        let bytes = reader.read(file)?;
                        w.copy_entry(file, bytes)?;
                    }
                    continue;
                }
                let rule = &cfg.targets[*rule];
                let out_name = translated_entry_name(&file.path, &cfg.general.target_lang, rule.kind);
                let zh_exists = out_name != file.path && names.contains(&out_name);
                let bytes = reader.read(file)?;
                if zh_exists && !opts.force {
                    report.files_skipped += 1;
                    if let Some(w) = writer.as_mut() {
                        w.copy_entry(file, bytes)?; // 已有中文文件：原样拷贝英文原文件
                    }
                    if let (Some(c), Some(rid)) = (&cache, run_id) {
                        let _ = c.set_file_state(rid, &file.path, kind_name(rule.kind), None, "skipped", 0, 0, None);
                    }
                    continue;
                }
                match process_file(&rctx, rule, &file.path, &bytes, &mut report).await {
                    Ok(Some(new_bytes)) => {
                        if let Some(w) = writer.as_mut() {
                            if cfg.general.mode == TranslateMode::Inplace
                                || rule.kind == FileKind::SnbtFields
                            {
                                // inplace / 章节文件：原地替换
                                w.write(&file.path, &new_bytes, file.method)?;
                            } else {
                                // zh_cn 增量模式：原文不动 + 新增中文语言文件
                                w.write(&file.path, &bytes, file.method)?;
                                w.write(&out_name, &new_bytes, file.method)?;
                            }
                        }
                        report.files_translated += 1;
                        if let (Some(c), Some(rid)) = (&cache, run_id) {
                            let _ = c.set_file_state(
                                rid, &file.path, kind_name(rule.kind), None, "done", 0, 0, None,
                            );
                        }
                    }
                    Ok(None) => {
                        // 无变化：只拷贝原文
                        if let Some(w) = writer.as_mut() {
                            w.copy_entry(file, bytes)?;
                        }
                        if let (Some(c), Some(rid)) = (&cache, run_id) {
                            let _ = c.set_file_state(
                                rid, &file.path, kind_name(rule.kind), None, "done", 0, 0, None,
                            );
                        }
                    }
                    Err(e) => {
                        report.files_failed += 1;
                        report.failures.push(FailureEntry {
                            container: file.path.clone(),
                            unit: "*".into(),
                            source: String::new(),
                            reason: format!("解析失败：{e}"),
                        });
                        if let Some(w) = writer.as_mut() {
                            w.copy_entry(file, bytes)?;
                        }
                        if let (Some(c), Some(rid)) = (&cache, run_id) {
                            let _ = c.set_file_state(
                                rid, &file.path, kind_name(rule.kind), None, "failed", 0, 0,
                                Some(&e.to_string()),
                            );
                        }
                    }
                }
            }
            Plan::Jar { outer, inners, bytes } => {
                if resume_done.contains(&outer.path) {
                    report.files_skipped += 1;
                    if let Some(w) = writer.as_mut() {
                        w.copy_entry(outer, bytes.clone())?;
                    }
                    continue;
                }
                match process_jar(&rctx, outer, inners, bytes, &names, &mut report).await {
                    Ok(new_bytes) => {
                        if let Some(w) = writer.as_mut() {
                            w.write(&outer.path, &new_bytes, outer.method)?;
                        }
                        report.files_translated += 1;
                        if let (Some(c), Some(rid)) = (&cache, run_id) {
                            let _ = c.set_file_state(
                                rid, &outer.path, "jar", None, "done",
                                inners.len() as i64, inners.len() as i64, None,
                            );
                        }
                    }
                    Err(e) => {
                        report.files_failed += 1;
                        report.failures.push(FailureEntry {
                            container: outer.path.clone(),
                            unit: "*".into(),
                            source: String::new(),
                            reason: format!("容器处理失败：{e}"),
                        });
                        if let Some(w) = writer.as_mut() {
                            w.copy_entry(outer, bytes.clone())?;
                        }
                        if let (Some(c), Some(rid)) = (&cache, run_id) {
                            let _ = c.set_file_state(
                                rid, &outer.path, "jar", None, "failed", 0, 0,
                                Some(&e.to_string()),
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- 收尾 ----
    report.cache_hits = counters.cache_hits.load(Ordering::Relaxed);
    report.translated = counters.translated.load(Ordering::Relaxed);
    report.placeholder_failures = counters.placeholder_failures.load(Ordering::Relaxed);
    report.duration_ms = started.elapsed().as_millis();
    let mut usage_map = usage.lock().unwrap();
    let mut list: Vec<ProviderUsage> = usage_map.drain().map(|(k, v)| ProviderUsage {
        provider: k,
        ..v
    }).collect();
    list.sort_by(|a, b| a.provider.cmp(&b.provider));
    report.provider_usage = list;

    if opts.dry_run {
        report.files_translated = 0;
    } else {
        if let Some(w) = writer.take() {
            let final_path = w.finish()?;
            report.output = final_path.display().to_string();
            let report_json = serde_json::to_string_pretty(&report).unwrap_or_default();
            let report_path = final_path.with_extension("report.json");
            std::fs::write(&report_path, &report_json).ok();
            std::fs::write(report_path.with_extension("md"), report.to_markdown()).ok();
        }
        if let (Some(c), Some(rid)) = (&cache, run_id) {
            let stats = serde_json::to_string(&report).unwrap_or_default();
            let status = if report.failures.is_empty() { "done" } else { "done_with_errors" };
            let _ = c.finish_run(rid, status, &stats);
        }
    }
    Ok(report)
}

/// 处理容器（jar/zip）：翻译内部命中条目并整包重建。
async fn process_jar(
    rctx: &RunCtx<'_>,
    outer: &PackFile,
    inners: &[(String, usize)],
    outer_bytes: &[u8],
    names: &HashSet<String>,
    report: &mut RunReport,
) -> McResult<Vec<u8>> {
    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();
    let mut additions: Vec<(String, Vec<u8>)> = Vec::new();
    for (inner, rule_idx) in inners {
        let rule = &rctx.cfg.targets[*rule_idx];
        let inner_full = format!("{}!/{}", outer.path, inner);
        let out_name = translated_entry_name(inner, &rctx.cfg.general.target_lang, rule.kind);
        // 已有中文语言文件时跳过（--force 可强制重译）
        let zh_exists = rule.kind != FileKind::SnbtFields
            && names.contains(&format!("{}!/{}", outer.path, out_name));
        if zh_exists && !rctx.force {
            report.files_skipped += 1;
            continue;
        }
        let bytes = match read_inner(outer_bytes, inner) {
            Ok(b) => b,
            Err(e) => {
                report.failures.push(FailureEntry {
                    container: inner_full.clone(),
                    unit: "*".into(),
                    source: String::new(),
                    reason: format!("读取容器条目失败：{e}"),
                });
                continue;
            }
        };
        match process_file(rctx, rule, &inner_full, &bytes, report).await {
            Ok(Some(new_bytes)) => {
                if rule.kind == FileKind::SnbtFields || rctx.cfg.general.mode == TranslateMode::Inplace {
                    replacements.insert(inner.clone(), new_bytes);
                } else {
                    additions.push((out_name, new_bytes));
                }
            }
            Ok(None) => {}
            Err(e) => {
                report.failures.push(FailureEntry {
                    container: inner_full,
                    unit: "*".into(),
                    source: String::new(),
                    reason: format!("解析失败：{e}"),
                });
            }
        }
    }
    rewrite_zip_container(outer_bytes, &replacements, &additions)
}

fn read_inner(outer_bytes: &[u8], inner: &str) -> McResult<Vec<u8>> {
    use std::io::Cursor;
    let mut ar = zip::ZipArchive::new(Cursor::new(outer_bytes.to_vec()))
        .map_err(|e| McError::Other(format!("容器不是有效 zip: {e}")))?;
    let mut e = ar
        .by_name(inner)
        .map_err(|e| McError::Other(format!("容器内找不到 {inner}: {e}")))?;
    let mut buf = Vec::with_capacity(e.size() as usize);
    std::io::Read::read_to_end(&mut e, &mut buf).map_err(|e| McError::Io {
        path: PathBuf::from("<container>"),
        source: e,
    })?;
    Ok(buf)
}

/// 处理单个文件：抽取任务 → 缓存 → 批次翻译 → 校验写回。
/// 返回 None 表示无变化（调用方拷贝原文）。
async fn process_file(
    rctx: &RunCtx<'_>,
    rule: &crate::config::TargetRule,
    container: &str,
    bytes: &[u8],
    report: &mut RunReport,
) -> McResult<Option<Vec<u8>>> {
    let analysis = analyze(rule.kind, &rule.text_keys, bytes)?;
    let cfg = rctx.cfg;

    // ---- 抽取任务 ----
    let mut unit_values: Vec<Option<String>> = vec![None; analysis.units.len()];
    let mut unit_changed = vec![false; analysis.units.len()];
    let mut tasks: Vec<Task> = Vec::new();
    for (ui, unit) in analysis.units.iter().enumerate() {
        report.units_total += 1;
        if unit.value.trim().is_empty() || !looks_translatable(&unit.value) || has_cjk(&unit.value) {
            continue;
        }
        if unit.comp {
            match component::text_units(&unit.value) {
                Ok(inner) => {
                    for iu in inner {
                        if iu.value.trim().is_empty() || !looks_translatable(&iu.value) || has_cjk(&iu.value) {
                            continue;
                        }
                        tasks.push(Task {
                            unit: ui,
                            comp: Some(iu.pointer.clone()),
                            source: iu.value.clone(),
                        });
                    }
                }
                Err(e) => {
                    report.failures.push(FailureEntry {
                        container: container.to_string(),
                        unit: unit.pointer.clone(),
                        source: unit.value.clone(),
                        reason: format!("组件解析失败：{e}"),
                    });
                }
            }
        } else {
            if unit.value.chars().count() > cfg.limits.max_entry_chars {
                report.too_long += 1;
                report.failures.push(FailureEntry {
                    container: container.to_string(),
                    unit: unit.pointer.clone(),
                    source: unit.value.chars().take(80).collect(),
                    reason: format!(
                        "超过 max_entry_chars={}，已跳过（v1.1 支持安全拆分）",
                        cfg.limits.max_entry_chars
                    ),
                });
                continue;
            }
            tasks.push(Task {
                unit: ui,
                comp: None,
                source: unit.value.clone(),
            });
        }
    }

    // ---- 缓存查询 ----
    let mut pending: Vec<usize> = Vec::new();
    for (ti, task) in tasks.iter().enumerate() {
        let mut hit: Option<String> = None;
        if let Some(c) = rctx.cache {
            if let Ok(Some(h)) = c.get(
                &cfg.general.src_lang,
                &cfg.general.target_lang,
                &task.source,
                Some((&rctx.active_provider, "")),
            ) {
                hit = Some(h.translated.clone());
            }
        }
        match hit {
            Some(t) => {
                rctx.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                stash(
                    &mut unit_values,
                    &mut unit_changed,
                    &analysis.units,
                    task,
                    &t,
                    container,
                    report,
                );
            }
            None => pending.push(ti),
        }
    }

    // ---- 批次翻译（掩码 → 渠道 → 还原校验 → 术语 → 缓存）----
    if !pending.is_empty() && !rctx.dry_run {
        let batches = split_batches(&tasks, &pending, cfg);
        let mut joins: JoinSet<(Vec<usize>, usize, McResult<(Vec<McResult<String>>, String)>)> = JoinSet::new();
        for b in batches {
            let permit = rctx.semaphore.clone().acquire_owned().await.unwrap();
            let runner = rctx.runner.clone();
            let ctx = rctx.req_ctx.clone();
            let breakers = rctx.breakers.clone();
            let engine = rctx.engine.clone();
            let cache = rctx.cache.clone();
            let glossary = rctx.glossary.clone();
            let active = rctx.active_provider.clone();
            let usage = rctx.usage.clone();
            let b_idx = b.clone();
            // 批次原文（未掩码），在闭包内完成掩码/还原
            let srcs: Vec<String> = b.iter().map(|&ti| tasks[ti].source.clone()).collect();
            let chars_in: usize = srcs.iter().map(|s| s.chars().count()).sum();
            let src_lang = cfg.general.src_lang.clone();
            let tgt_lang = cfg.general.target_lang.clone();
            joins.spawn(async move {
                let _permit = permit;
                // 1. 掩码
                let mut guards = Vec::with_capacity(srcs.len());
                let mut masked = Vec::with_capacity(srcs.len());
                for s in &srcs {
                    let g = engine.guard(s);
                    masked.push(g.mask(s));
                    guards.push(g);
                }
                // 2. 渠道翻译（内部重试/回退）
                let out = match runner.run(&masked, &ctx, &breakers).await {
                    Ok(o) => o,
                    Err(e) => return (b_idx, chars_in, Err(e)),
                };
                {
                    let mut u = usage.lock().unwrap();
                    let slot = u.entry(out.provider.clone()).or_default();
                    slot.batches += 1;
                    slot.chars_in += chars_in;
                    slot.chars_out += out.translations.iter().map(|s| s.chars().count()).sum::<usize>();
                }
                // 3. 逐条还原校验 + 术语 + 缓存
                let mut results = Vec::with_capacity(srcs.len());
                for (i, t) in out.translations.into_iter().enumerate() {
                    match guards[i].restore(&t, &srcs[i]) {
                        Ok(restored) => {
                            let final_text = apply_glossary(&restored, &glossary, guards[i].raws());
                            if let Some(c) = &cache {
                                let _ = c.put(
                                    &src_lang,
                                    &tgt_lang,
                                    &srcs[i],
                                    &final_text,
                                    &active,
                                    "",
                                    true,
                                );
                            }
                            results.push(Ok(final_text));
                        }
                        Err(e) => results.push(Err(e)),
                    }
                }
                (b_idx, chars_in, Ok((results, out.provider)))
            });
        }
        while let Some(joined) = joins.join_next().await {
            match joined {
                Ok((b_idx, _, Ok((results, _)))) => {
                    for (i, r) in results.into_iter().enumerate() {
                        let task = &tasks[b_idx[i]];
                        match r {
                            Ok(final_text) => {
                                rctx.counters.translated.fetch_add(1, Ordering::Relaxed);
                                stash(
                                    &mut unit_values,
                                    &mut unit_changed,
                                    &analysis.units,
                                    task,
                                    &final_text,
                                    container,
                                    report,
                                );
                            }
                            Err(e) => {
                                rctx.counters.placeholder_failures.fetch_add(1, Ordering::Relaxed);
                                report.failures.push(FailureEntry {
                                    container: container.to_string(),
                                    unit: task.comp.clone().unwrap_or_else(|| {
                                        analysis.units[task.unit].pointer.clone()
                                    }),
                                    source: task.source.chars().take(80).collect(),
                                    reason: format!("占位符校验失败，保留原文：{e}"),
                                });
                            }
                        }
                    }
                }
                Ok((_, _, Err(e))) => {
                    rctx.counters.failed.fetch_add(1, Ordering::Relaxed);
                    report.failures.push(FailureEntry {
                        container: container.to_string(),
                        unit: "*".into(),
                        source: String::new(),
                        reason: format!("批次翻译失败：{e}"),
                    });
                }
                Err(e) => {
                    report.failures.push(FailureEntry {
                        container: container.to_string(),
                        unit: "*".into(),
                        source: String::new(),
                        reason: format!("任务崩溃：{e}"),
                    });
                }
            }
        }
    } else if rctx.dry_run && !pending.is_empty() {
        report.would_translate += pending.len();
    }

    // ---- 组件重组 + 保形写回 ----
    let mut updates: Vec<(String, String)> = Vec::new();
    for (ui, unit) in analysis.units.iter().enumerate() {
        if !unit_changed[ui] {
            continue;
        }
        let new_value = match &unit_values[ui] {
            Some(v) => v.clone(),
            None => continue,
        };
        updates.push((unit.pointer.clone(), new_value));
    }
    if updates.is_empty() {
        return Ok(None);
    }
    let new_text = apply_updates(rule.kind, &analysis.text, &rule.text_keys, &updates)?;
    Ok(Some(encoding::encode(&new_text, analysis.enc, analysis.bom)))
}

/// 把某个 task 的译文写回单元值（组件单元重组，普通单元直接替换）。
fn stash(
    unit_values: &mut [Option<String>],
    unit_changed: &mut [bool],
    units: &[pack::Unit],
    task: &Task,
    translated: &str,
    container: &str,
    report: &mut RunReport,
) {
    let unit = &units[task.unit];
    match &task.comp {
        None => {
            unit_values[task.unit] = Some(translated.to_string());
            unit_changed[task.unit] = true;
        }
        Some(cp) => {
            let base = match &unit_values[task.unit] {
                Some(v) => v.clone(),
                None => unit.value.clone(),
            };
            match component::rewrite_texts(&base, &[(cp.clone(), translated.to_string())]) {
                Ok(v) => {
                    unit_values[task.unit] = Some(v);
                    unit_changed[task.unit] = true;
                }
                Err(e) => {
                    report.failures.push(FailureEntry {
                        container: container.to_string(),
                        unit: format!("{}/{}", unit.pointer, cp),
                        source: translated.to_string(),
                        reason: format!("组件重组失败：{e}"),
                    });
                }
            }
        }
    }
}

/// 术语后处理：只替换占位符之外的片段，避免误伤颜色码等。
fn apply_glossary(text: &str, terms: &[(String, String)], raws: &[String]) -> String {
    if terms.is_empty() || raws.is_empty() {
        return text.to_string();
    }
    let mut pieces: Vec<&str> = vec![text];
    let mut seps: Vec<String> = Vec::new();
    for raw in raws {
        let mut next_pieces: Vec<&str> = Vec::new();
        let mut next_seps: Vec<String> = Vec::new();
        for (i, p) in pieces.iter().enumerate() {
            if i > 0 {
                next_seps.push(seps[i - 1].clone());
            }
            let parts: Vec<&str> = p.split(raw.as_str()).collect();
            for (j, seg) in parts.iter().enumerate() {
                if j > 0 {
                    next_seps.push(raw.clone());
                }
                next_pieces.push(seg);
            }
        }
        pieces = next_pieces;
        seps = next_seps;
    }
    let replace = |s: &str| -> String {
        let mut out = s.to_string();
        for (k, v) in terms {
            if !k.is_empty() && out.contains(k.as_str()) {
                out = out.replace(k.as_str(), v.as_str());
            }
        }
        out
    };
    let mut out = String::new();
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.push_str(&seps[i - 1]);
        }
        out.push_str(&replace(p));
    }
    out
}

/// 按数量/字符限制把待译任务切批。
fn split_batches(tasks: &[Task], pending: &[usize], cfg: &Config) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_chars = 0usize;
    for &ti in pending {
        let len = tasks[ti].source.chars().count();
        if !cur.is_empty()
            && (cur.len() >= cfg.limits.max_batch_texts
                || cur_chars + len > cfg.limits.max_batch_chars)
        {
            out.push(std::mem::take(&mut cur));
            cur_chars = 0;
        }
        cur.push(ti);
        cur_chars += len;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn kind_name(k: FileKind) -> &'static str {
    match k {
        FileKind::Json => "json",
        FileKind::Lang => "lang",
        FileKind::SnbtMap => "snbt",
        FileKind::SnbtFields => "snbt",
    }
}

/// 只扫描不翻译：统计可译文件、单元数、缓存命中与预计翻译量。
pub async fn scan(cfg: &Config, opts: &TranslateOpts) -> McResult<ScanReport> {
    let reader = PackReader::open(&opts.input)?;
    let files = reader.files()?;
    let mut rep = ScanReport::default();
    rep.pack = opts.input.display().to_string();
    let cache = if cfg.general.cache && !opts.no_cache {
        Some(Cache::open(&config::resolve_db_path(cfg))?)
    } else {
        None
    };
    for file in &files {
        if let Some(rule) = match_target(&cfg.targets, &file.path) {
            let rule = &cfg.targets[rule];
            rep.files_matched += 1;
            let bytes = match reader.read(file) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Ok(a) = analyze(rule.kind, &rule.text_keys, &bytes) {
                for u in &a.units {
                    rep.units_total += 1;
                    // 组件单元展开为内部 text 与 translate 的统计口径保持一致
                    let texts: Vec<String> = if u.comp {
                        match component::text_units(&u.value) {
                            Ok(inner) => inner.into_iter().map(|i| i.value).collect(),
                            Err(_) => vec![u.value.clone()],
                        }
                    } else {
                        vec![u.value.clone()]
                    };
                    for t in &texts {
                        rep.chars_total += t.chars().count();
                        if t.trim().is_empty() || !looks_translatable(t) || has_cjk(t) {
                            continue;
                        }
                        let cached = match &cache {
                            Some(c) => c
                                .get(&cfg.general.src_lang, &cfg.general.target_lang, t, None)?
                                .is_some(),
                            None => false,
                        };
                        if cached {
                            rep.cache_hits += 1;
                        } else {
                            rep.to_translate += 1;
                        }
                    }
                }
            }
        }
    }
    Ok(rep)
}
