//! mctrans CLI：scan / translate / cache / config。

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use mctrans_core::cache::{Cache, ExportRecord};
use mctrans_core::config::{self, Config, TranslateMode};
use mctrans_core::pipeline::{self, TranslateOpts};

#[derive(Parser)]
#[command(
    name = "mctrans",
    version,
    about = "MC 整合包翻译工具：FTB Quests 与 Mod 语言文件批量翻译",
    long_about = "支持 DeepL 与 OpenAI 兼容 API 双渠道，可切换、可回退；SQLite 缓存避免重复请求；\n占位符（§6 颜色码、%s、{0} 等）全程保护，译后多重集校验。"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// 配置文件路径（默认：./mctrans.toml，其次用户数据目录）
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// 日志级别：error/warn/info/debug/trace
    #[arg(long, global = true, default_value = "warn")]
    log_level: String,
}

#[derive(Subcommand)]
enum Commands {
    /// 扫描整合包：列出可译文件、单元数、缓存命中与预计翻译量
    Scan {
        /// 整合包 zip 或已解压目录
        pack: PathBuf,
        /// JSON 输出（供前端/脚本消费）
        #[arg(long)]
        json: bool,
    },
    /// 翻译整合包
    Translate {
        pack: PathBuf,
        /// 输出路径（默认：输入名-目标语言.zip/.目录）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 切换翻译渠道并持久化（写入配置，立即生效）
        #[arg(long)]
        provider: Option<String>,
        /// zh_cn（新增中文文件）或 inplace（原地替换）
        #[arg(long)]
        mode: Option<String>,
        /// 覆盖已存在的输出
        #[arg(long)]
        force: bool,
        /// 只分析不翻译、不调 API
        #[arg(long)]
        dry_run: bool,
        /// 不走缓存，强制重新翻译
        #[arg(long)]
        no_cache: bool,
        /// 断点续跑：跳过上次已完成的文件
        #[arg(long)]
        resume: bool,
        /// 并发批次数
        #[arg(short, long)]
        concurrency: Option<usize>,
    },
    /// 缓存管理
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// 术语表管理
    Glossary {
        #[command(subcommand)]
        cmd: GlossaryCmd,
    },
    /// 配置管理
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// 统计
    Stats,
    /// 导出为 JSONL
    Export { file: PathBuf },
    /// 从 JSONL 导入
    Import { file: PathBuf },
    /// 清空（--scope all 连运行记录/术语表一起清）
    Clear {
        #[arg(long, default_value = "translations")]
        scope: String,
    },
    /// 完整性自检
    Check,
}

#[derive(Subcommand)]
enum GlossaryCmd {
    List,
    /// 新增术语
    Add { src: String, tgt: String },
    /// 删除术语
    Remove { src: String, tgt: String },
    /// 从 CSV/TSV 导入（src,tgt）
    Import { file: PathBuf },
    /// 导出为 CSV
    Export { file: PathBuf },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// 生成带注释的示例配置
    Init {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// 显示当前生效配置（脱敏）
    Show,
    /// 切换当前翻译渠道（写入配置，之后所有 run 默认使用）
    Use { provider: String },
    /// 测试渠道连通性（发送一次 Ping）
    Test { provider: Option<String> },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let level = match cli.log_level.as_str() {
        "error" => "error",
        "info" => "info",
        "debug" => "debug",
        "trace" => "trace",
        _ => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .init();

    if let Err(e) = run(&cli).await {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}

async fn run(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Commands::Scan { pack, json } => {
            let cfg = Config::load(&config::Config::locate(cli.config.as_deref()))?;
            let rep = pipeline::scan(&cfg, &opts_for_scan(pack, cli)).await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!("整合包：{}", rep.pack);
                println!("命中目标文件：{}", rep.files_matched);
                println!("待译单元：{}（共 {} 字符）", rep.units_total, rep.chars_total);
                println!("缓存命中：{}", rep.cache_hits);
                println!("预计翻译：{}", rep.to_translate);
            }
        }
        Commands::Translate {
            pack,
            output,
            provider,
            mode,
            force,
            dry_run,
            no_cache,
            resume,
            concurrency,
        } => {
            let cfg_path = config::Config::locate(cli.config.as_deref());
            let mut cfg = Config::load(&cfg_path)?;
            if let Some(mode) = mode {
                cfg.general.mode = match mode.as_str() {
                    "zh_cn" => TranslateMode::ZhCn,
                    "inplace" => TranslateMode::Inplace,
                    other => anyhow::bail!("未知模式 {other}（可选 zh_cn / inplace）"),
                };
            }
            if let Some(p) = concurrency {
                cfg.general.concurrency = (*p).max(1);
            }
            // 渠道切换：持久化到配置，立即生效于本次 run
            if let Some(name) = provider {
                if !cfg.providers.contains_key(name) {
                    anyhow::bail!(
                        "配置中没有渠道 {name:?}。可选：{:?}",
                        cfg.providers.keys().collect::<Vec<_>>()
                    );
                }
                config::persist_active_provider(&cfg_path, name)?;
                cfg.general.active_provider = Some(name.clone());
                println!("已切换翻译渠道 → {name}（已写入 {}，立即生效）", cfg_path.display());
            }
            let cb: Option<Arc<dyn Fn(String) + Send + Sync>> = Some(Arc::new(|msg: String| {
                println!("[mctrans] {msg}");
            }));
            let opts = TranslateOpts {
                input: pack.clone(),
                output: output.clone(),
                dry_run: *dry_run,
                force: *force,
                no_cache: *no_cache,
                resume: *resume,
                concurrency: cfg.general.concurrency,
                provider_override: provider.clone(),
                config_path: cli.config.clone(),
                system_suffix: None,
                // 取消：Tauri/脚本可通过各自宿主控制；CLI 下 Ctrl-C 直接终止，
                // 由于逐文件原子提交，已生成文件始终完整，可用 --resume 续跑。
                cancel: None,
                on_event: cb,
            };
            let rep = pipeline::translate(&cfg, &opts).await?;
            if *dry_run {
                println!("dry-run 结果（未写入文件）：");
                println!("  命中文件 {}，单元 {}，缓存命中 {}，预计翻译 {}",
                    rep.files_total, rep.units_total, rep.cache_hits, rep.would_translate);
            } else {
                println!("完成：{}", rep.output);
                println!(
                    "  文件 {}/{}，单元 {}，缓存 {}，翻译 {}，占位符保护失败 {}，失败项 {}，耗时 {} ms",
                    rep.files_translated,
                    rep.files_total,
                    rep.units_total,
                    rep.cache_hits,
                    rep.translated,
                    rep.placeholder_failures,
                    rep.failures.len(),
                    rep.duration_ms
                );
                if !rep.failures.is_empty() {
                    println!("  失败明细见报告：{}.report.md", rep.output);
                }
            }
        }
        Commands::Cache { cmd } => {
            let cfg = Config::load(&config::Config::locate(cli.config.as_deref()))?;
            let cache = Cache::open(&config::resolve_db_path(&cfg))?;
            match cmd {
                CacheCmd::Stats => {
                    let s = cache.stats()?;
                    println!("原文条目：{}", s.sources);
                    println!("译文明细：{}", s.translations);
                    println!("术语条目：{}", s.glossary);
                    println!("历史 run：{}", s.runs);
                    for (p, m, n) in &s.by_provider {
                        println!("  渠道 {p}（{m}）：{n} 条");
                    }
                }
                CacheCmd::Export { file } => {
                    let data = cache.export_jsonl()?;
                    let text = data
                        .iter()
                        .map(|r| serde_json::to_string(r).unwrap())
                        .collect::<Vec<_>>()
                        .join("\n");
                    std::fs::write(file, text)?;
                    println!("已导出 {} 条到 {}", data.len(), file.display());
                }
                CacheCmd::Import { file } => {
                    let text = std::fs::read_to_string(file)?;
                    let mut records = Vec::new();
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        records.push(serde_json::from_str::<ExportRecord>(line)?);
                    }
                    let n = cache.import_jsonl(&records)?;
                    println!("已导入 {n} 条");
                }
                CacheCmd::Clear { scope } => {
                    cache.clear(scope)?;
                    println!("缓存已清空（scope={scope}）");
                }
                CacheCmd::Check => {
                    cache.integrity_check()?;
                    println!("数据库完整性 OK");
                }
            }
        }
        Commands::Glossary { cmd } => {
            let cfg = Config::load(&config::Config::locate(cli.config.as_deref()))?;
            let cache = Cache::open(&config::resolve_db_path(&cfg))?;
            match cmd {
                GlossaryCmd::List => {
                    for (s, t) in cache.glossary_list()? {
                        println!("{s}\t{t}");
                    }
                }
                GlossaryCmd::Add { src, tgt } => {
                    cache.glossary_upsert(src, tgt)?;
                    println!("已添加术语：{src} ⇒ {tgt}");
                }
                GlossaryCmd::Remove { src, tgt } => {
                    if cache.glossary_delete(src, tgt)? {
                        println!("已删除术语：{src} ⇒ {tgt}");
                    } else {
                        println!("术语不存在：{src} ⇒ {tgt}");
                    }
                }
                GlossaryCmd::Import { file } => {
                    let text = std::fs::read_to_string(file)?;
                    let mut n = 0;
                    for (i, line) in text.lines().enumerate() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let sep = if line.contains('\t') { '\t' } else { ',' };
                        let parts: Vec<&str> = line.splitn(2, sep).collect();
                        if parts.len() == 2 {
                            cache.glossary_upsert(parts[0].trim(), parts[1].trim())?;
                            n += 1;
                        } else {
                            eprintln!("第 {} 行格式不符（应为 src{sep}tgt）：{line}", i + 1);
                        }
                    }
                    println!("已导入 {n} 条术语");
                }
                GlossaryCmd::Export { file } => {
                    let mut out = String::from("src,tgt\n");
                    for (s, t) in cache.glossary_list()? {
                        out.push_str(&format!("{s},{t}\n"));
                    }
                    std::fs::write(file, out)?;
                    println!("已导出到 {}", file.display());
                }
            }
        }
        Commands::Config { cmd } => {
            match cmd {
                ConfigCmd::Init { path } => {
                    let p = path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(config::DEFAULT_CONFIG_NAME));
                    if p.exists() {
                        println!("{} 已存在，未覆盖", p.display());
                        return Ok(());
                    }
                    std::fs::write(&p, include_str!("mctrans.toml.example"))?;
                    println!("已生成示例配置：{}", p.display());
                    println!("请编辑 providers 段并设置对应 API Key 环境变量");
                }
                ConfigCmd::Show => {
                    let cfg = Config::load(&config::Config::locate(cli.config.as_deref()))?;
                    show_config(&cfg, cli.config.as_deref());
                }
                ConfigCmd::Use { provider } => {
                    let cfg_path = config::Config::locate(cli.config.as_deref());
                    let cfg = Config::load(&cfg_path)?;
                    if !cfg.providers.contains_key(provider) {
                        anyhow::bail!(
                            "配置中没有渠道 {provider:?}。可选：{:?}",
                            cfg.providers.keys().collect::<Vec<_>>()
                        );
                    }
                    config::persist_active_provider(&cfg_path, provider)?;
                    println!("已切换翻译渠道 → {provider}（已写入 {cfg_path:?}）", cfg_path = cfg_path.display());
                    println!("回退链：{}", cfg.fallback.chain.join(" → "));
                }
                ConfigCmd::Test { provider } => {
                    let cfg = Config::load(&config::Config::locate(cli.config.as_deref()))?;
                    let order = cfg.provider_order(provider.as_deref());
                    if order.is_empty() {
                        anyhow::bail!("没有配置任何翻译渠道");
                    }
                    let mut any_ok = false;
                    for name in &order {
                        let pcfg = &cfg.providers[name];
                        match mctrans_core::providers::build_provider(name, pcfg, cfg.resolve_api_key(name)) {
                            Ok(p) => match mctrans_core::providers::test_provider(&p).await {
                                Ok(reply) => {
                                    println!("渠道 {name} 连通正常（Ping → {reply:?}）");
                                    any_ok = true;
                                }
                                Err(e) => println!("渠道 {name} 测试失败：{e}"),
                            },
                            Err(e) => println!("渠道 {name} 初始化失败：{e}"),
                        }
                    }
                    if !any_ok {
                        anyhow::bail!("所有渠道均不可用");
                    }
                }
            }
        }
    }
    Ok(())
}

fn opts_for_scan(pack: &PathBuf, cli: &Cli) -> TranslateOpts {
    TranslateOpts {
        input: pack.clone(),
        output: None,
        dry_run: true,
        force: false,
        no_cache: false,
        resume: false,
        concurrency: 1,
        provider_override: None,
        config_path: cli.config.clone(),
        system_suffix: None,
        cancel: None,
        on_event: None,
    }
}

fn show_config(cfg: &Config, explicit: Option<&std::path::Path>) {    let path = config::Config::locate(explicit);
    println!("配置文件：{}", path.display());
    println!("语言方向：{} → {}", cfg.general.src_lang, cfg.general.target_lang);
    println!(
        "模式：{}",
        match cfg.general.mode {
            TranslateMode::ZhCn => "zh_cn（新增中文语言文件）",
            TranslateMode::Inplace => "inplace（原地替换）",
        }
    );
    println!(
        "当前渠道：{}",
        cfg.active_name(None).unwrap_or_else(|| "<未配置>".into())
    );
    let key = |name: &str| -> String {
        cfg.resolve_api_key(name)
            .map(|k| {
                let mask = if k.len() > 6 {
                    format!("{}…{}", &k[..3], &k[k.len() - 3..])
                } else {
                    "***".into()
                };
                format!("{mask}（已加载）")
            })
            .unwrap_or_else(|| "<未设置>".into())
    };
    for (name, p) in &cfg.providers {
        println!(
            "渠道 {name}：type={:?} base_url={} model={} key={}",
            p.kind,
            p.base_url,
            p.model.clone().unwrap_or_else(|| "-".into()),
            key(name)
        );
    }
    println!("回退链：{}", cfg.fallback.chain.join(" → "));
    println!("数据库：{}（{}）", config::resolve_db_path(cfg).display(), if cfg.general.cache { "缓存开" } else { "缓存关" });
}
