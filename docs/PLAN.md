# mctrans — MC 整合包翻译工具 开发规划

**状态：待用户确认 | 版本：v0.1-plan | 日期：2026-10-18**

本文档只做规划，不进入编码。确认（或提出修改）后按第 16 节里程碑动工。

---

## 1. 项目定位

- 名称：`mctrans`（工作名，可改）
- 一句话：把 Minecraft 整合包里的英文文本（FTB Quests 任务、Mod 语言文件）批量翻译成简体中文，产出结构一致、可直接进游戏的中文整合包。

输入：

- `*.zip` 整合包（CurseForge / Modrinth 导出格式）
- 已解压目录

输出：

- 默认模式（`zh_cn`）：新增 `zh_cn.json` / `zh_cn.lang` / `zh_cn.snbt` 等中文语言文件，原文不动，游戏切换到中文后自动加载
- `inplace` 模式：原地替换原语言文件内容（服务端强制中文场景）
- 附运行报告（JSON + Markdown）：翻译/跳过/失败计数、占位符告警、失败条目明细、各 Provider 用量

四条硬约束贯穿整个设计：

1. 翻译后文件必须在游戏里正常加载——任何破坏格式的改动都视为 bug
2. 游戏内格式化占位符（颜色代码、`%s`、`{0}` 等）翻译后必须原样保留且数量一致
3. 已翻译过的内容不重复请求 API（SQL 缓存）
4. 中断后能从断点继续，不重复劳动，不产出半个坏包

---

## 2. 目标与非目标

v1 目标：

1. 解析并翻译：jar 内 `assets/*/lang/en_us.json`（含 kubejs 资源域）、legacy `*.lang`、`config/ftbquests/**` 下的 snbt（lang 映射表 + chapter / quest 的文本字段）
2. 翻译源：DeepL v2 API + OpenAI 兼容 API（chat/completions），可配置优先级、故障回退
3. SQLite 数据库缓存，内容寻址，跨包去重；run 级断点续跑
4. 占位符保护：识别、掩码、翻译后校验、自动修复、修复失败则保留原文并记录
5. CLI（clap）+ Tauri 桌面 GUI（React + TS）+ NSIS 一键安装器
6. 中文文档：`README.MD` + `docs/`（架构、配置、使用、占位符说明、安装器）

非目标（v1 不做，预留扩展位）：

- Minecraft 本体汉化、基岩版
- 图片内嵌文字 OCR
- 术语表云端协作
- MySQL / PostgreSQL（数据层走 repository trait 抽象，v2 再接）

---

## 3. 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| 核心库 / CLI | Rust 2021 + tokio + clap 4 + serde | 单二进制、跨平台、类型安全，jar/zip 流式处理性能足够 |
| HTTP | reqwest（rustls，无 OpenSSL 依赖） | 超时/连接池/重试可控，跨平台省心 |
| 数据库 | SQLite（rusqlite bundled + WAL） | 零部署单文件；repository trait 预留换库能力 |
| 压缩 | zip | 条目级流式复制，服务端大包不爆内存 |
| 编码 | encoding_rs | GBK/Big5/UTF-8 互转，带 BOM 处理 |
| 前端 | Tauri v2 + React + TypeScript + Vite | TS 只写前端；Rust 核心通过 #[tauri::command] 复用；bundler 直接产安装器 |
| 测试 | cargo test + wiremock + vitest | Provider 行为全部模拟，不依赖真实 API |

关键取舍：**不在解压后的目录上工作，也不解压重打包 jar**。zip 读写全部流式：读旧包 → 逐条目拷贝 → 命中规则的条目替换内容 → 写新文件 → 原子改名。crc、压缩方式、其他条目保持原样。

---

## 4. 总体架构

### 4.1 仓库结构

```
D:/MCfanyi/
├── Cargo.toml                  # workspace
├── crates/
│   ├── mctrans-core/           # 核心库：解析、占位符、providers、缓存、管线
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── error.rs        # 统一错误类型（分层）
│   │   │   ├── config.rs       # TOML 配置：加载、合并、校验、默认值
│   │   │   ├── text/           # 结构编辑器（见 4.3）
│   │   │   │   ├── json_edit.rs
│   │   │   │   ├── snbt.rs
│   │   │   │   ├── lang.rs
│   │   │   │   ├── encoding.rs
│   │   │   │   └── component.rs  # FTB Quests 文本组件 JSON（嵌套 {"text":...}）
│   │   │   ├── placeholder/    # 占位符守卫（第 6 节）
│   │   │   ├── providers/      # deepl.rs / openai.rs / mod.rs(trait+fallback)
│   │   │   ├── cache/          # schema.rs / repo.rs / glossary.rs
│   │   │   ├── pack/           # 扫描、目标规则、jar 流式读写
│   │   │   ├── pipeline/       # 编排、断点续跑、进度事件
│   │   │   └── report.rs
│   ├── mctrans-cli/            # clap 子命令二进制
│   └── mctrans-tauri/          # Tauri 命令层（薄封装，调用 core，事件转发到前端）
├── ui/                         # Vite + React + TS 前端
├── installer/                  # NSIS 定制脚本、便携版打包
├── docs/                       # 本文档及后续文档
├── samples/                    # 测试用迷你整合包（小体积，进 git）
├── tests/                      # 集成测试 / e2e
└── README.MD
```

### 4.2 数据流（管线）

```
扫描 pack ──► 命中规则的目标文件清单
                │
                ▼
        结构编辑器解析文件 ──► 抽取待译单元（原文，带定位）
                │
                ▼
        占位符掩码（⟦0⟧ ⟦1⟧ …）
                │
                ▼
    ┌──────► 缓存查询（src+tgt+sha256）
    │            │命中               │未命中
    │            ▼                   ▼
    │        直接采用           Provider Fallback Chain
    │                        deepl ──失败──► openai ──失败──► (下一个)
    │                            │ 429/456/5xx/超时/坏返回
    │                            ▼
    │                     重试(退避)/二分批次/切换
    ▼            │
  校验与修复 ◄────┘（占位符多重集比对 → 修复 → 失败则保留原文并记录）
    │
    ▼
  解除掩码 → 回写缓存 → 该文件所有单元通过后原子提交该文件
    │
    ▼
  下一个文件（每个文件独立提交；run 状态落库，可 resume）
```

### 4.3 核心模块职责

| 模块 | 职责 |
|---|---|
| `text/json_edit.rs` | JSON 结构编辑器：抬手只改字符串 value 的字符区间，缩进、键顺序、尾逗号、转义风格全部保留。写完必须能重新 parse 回去才算成功 |
| `text/snbt.rs` | SNBT 结构编辑器：词法级扫描（键/值/引号状态/转义），只重写目标 key 的字符串值；`lang` 映射表模式整表值替换 |
| `text/lang.rs` | legacy `.lang`：按行解析，保留注释、空行、键顺序、重复键（Java 语义取最后一个）；转义/反转义 |
| `text/encoding.rs` | BOM/UTF-8 严格校验/GBK/Big5 探测，写回时用原编码，探测结果记库 |
| `text/component.rs` | 字符串若是 JSON 文本组件（`{"text":"...","extra":[...]}`），只译 `text` 与 `extra` 里的字符串，其他键（color、hoverEvent 等）原样；解析失败降级为整串翻译 + 写回前 JSON 复检 |
| `placeholder/*` | 占位符规则集、掩码/解掩码、校验、修复（第 6 节） |
| `providers/*` | Translator trait、DeepL / OpenAI 兼容实现、fallback chain、令牌桶限流、简单熔断 |
| `cache/*` | SQLite 单写者 actor、DDL 迁移、术语表增删查 |
| `pack/*` | zip 流式读写（含 jar 条目原地替换）、目标规则匹配（glob + kind + 字段白名单） |
| `pipeline/*` | 编排器、断点续跑、取消（CancellationToken）、进度事件（broadcast channel） |
| `report.rs` | 运行报告（JSON + Markdown） |

错误类型分层（`error.rs`，thiserror）：`Config / Io / Zip / Json / Sql / Http / Decode / PlaceholderMismatch / RateLimited / QuotaExhausted / ProviderUnavailable / OutputLocked / Cancelled`。每个变体携带定位信息（文件路径、条目、原文摘要），最终汇总进报告。

---

## 5. 可翻译内容识别

规则驱动（config.toml，可按包增删）：

```toml
[[targets]]
name = "mod-lang-json"
kind = "json"
match = ["mods/**/assets/*/lang/en_us.json", "kubejs/assets/*/lang/en_us.json", "resources/**/assets/*/lang/en_us.json"]

[[targets]]
name = "mod-lang-legacy"
kind = "lang"
match = ["mods/**/assets/*/lang/en_us.lang"]

[[targets]]
name = "ftb-quests-lang-snbt"
kind = "snbt_map"
match = ["config/ftbquests/quests/lang/*.snbt"]

[[targets]]
name = "ftb-quests-lang-json"
kind = "json"
match = ["config/ftbquests/quests/lang/*.json"]

[[targets]]
name = "ftb-quests-chapters"
kind = "snbt_fields"
match = ["config/ftbquests/quests/chapters/**/*.snbt"]
text_keys = ["title", "subtitle", "description", "text"]
```

跳过策略（不送译，直接保留/拷贝）：

- 空串、纯空白、单标点、纯占位符串
- 原文已含 CJK 字符（说明已是中文或本地化文本，en_us 来源不应出现）
- 目标语言文件已存在且未指定 `--force`
- 规则未覆盖的文件一律原样拷贝

`snbt_fields` 说明：FTB Quests 各版本的 chapter/task snbt 里 `title`、`subtitle`、`description` 字段多数是字面英文或 JSON 文本组件字符串。编辑器按 key 白名单定位，命中后走 `component.rs` 判断是 JSON 组件还是普通串。**key 白名单可配置**，遇到新版本文档外字段只拷贝不改，不会静默改坏结构。

---

## 6. 占位符保护策略（本项目的核心）

### 6.1 占位符分类与规则

| ID | 类别 | 示例 | 正则（默认开启，可配置） |
|---|---|---|---|
| P1 | MC 原版 § 代码 | `§6黄金§r`、`§l§n` | `§[0-9a-fk-or]`（忽略大小写） |
| P2 | MC 十六进制颜色 | `§x§7§f§0§0§0§0` | 由 P1 逐段覆盖 |
| P3 | Bukkit & 颜色代码 | `&6&l消息` | `&[0-9a-fk-or]`（可开关） |
| P4 | printf 风格 | `%s`、`%1$d`、`%.2f`、`%%` | `%(\d+\$)?[-#+ 0,(]*\d*(\.\d+)?[sdifgeEuoxXcn%]` |
| P5 | MessageFormat | `{0}`、`{1,number}` | `\{\d+([,}][^}]*)?\}` |
| P6 | 转义序列 | `\n` `\t` `\r` `\\` `\"` | `\\[nrt\\"']` |
| P7 | 内联标签/实体 | `<br>` `<b>…</b>` `&amp;` | `</?[A-Za-z][^>]*>\|&[A-Za-z]+;\|&#\d+;` |
| P8 | 用户自定义 | config: `custom_patterns` | 原样透传 |

### 6.2 掩码机制

送译前把每处占位符替换为唯一标记（sentinel），API 返回后还原：

- 默认 sentinel：`⟦0⟧`、`⟦1⟧` …（config 可改，如 `@@0@@`；某些模型对生僻符号更友好，实测后定默认值）
- 注册表：index → 原文（含原字符），掩码后文本与注册表成对传递
- 掩码只作用于送译字符串；文件其它部分（键、结构、注释）从不经过 API

例：`"&6Cost: %1$s gold"` → 掩码 → `"⟦0⟧Cost: ⟦1⟧ gold"` → 译文 `"⟦0⟧花费：⟦1⟧ 黄金"` → 还原 → `"&6花费：⟦?⟧"` 即 `"&6花费：%1$s 黄金"`。

### 6.3 校验与修复

翻译完成后按序执行：

1. **多重集比对**：原文占位符与译文占位符的数量集合必须一致
2. **有序敏感检查**：带序号的（`%1$s`、`{0}`）相对顺序不得改变；乱序但数量对 → 告警放行；数量不对 → 进入修复
3. **修复算法**：
   - 数量相同但内容被改（`%s`→`%d`）：按位替换回原文占位符
   - 数量不同：尝试将缺失的占位符按原位置插回；无法安全插回 → 放弃该译文
4. **兜底**：放弃的条目保留原文，写入报告 `placeholder_mismatch` 明细，可选带更强 system prompt 重试一次
5. **首尾空白保护**：比对译文首尾空白，丢失则按原文补齐
6. **JSON 写回前复检**：凡经过 JSON 组件路径的译文，写回前重新 parse，失败即弃用该译文

### 6.4 超长文本

- 单条超 `max_entry_chars`（默认 8000 字符）：按句子边界拆分——**只在占位符之外找断点**，掩码后的 sentinel 作为原子单元不切断；分段独立送译、独立缓存键为分段内容，组装后整条缓存
- 单条超硬上限：标记失败并报告，不让 API 直接报错
- 批次按 `max_batch_texts` × `max_batch_chars`（默认 40 条 / 20000 字符）切批，超限自动二分

---

## 7. 翻译 Provider 与回退

### 7.1 统一抽象

```rust
#[async_trait]
pub trait Translator: Send + Sync {
    fn name(&self) -> &str;
    fn max_batch_texts(&self) -> usize;
    fn max_batch_chars(&self) -> usize;
    async fn translate(&self, batch: &[String], ctx: &TranslateCtx)
        -> ProviderResult<Vec<String>>;
}
```

### 7.2 DeepL v2

- 端点：`POST {base_url}/v2/translate`，免费版 `base_url = https://api-free.deepl.com`
- 鉴权：`Authorization: DeepL-Auth-Key <key>`（key 从 env 或系统 keychain 读，不落明文）
- 请求：`{ "text": [...], "source_lang": "EN", "target_lang": "ZH", "formality": "default", "preserve_formatting": false }`（占位符已由我们保护，`tag_handling` 不开）
- 限制：单批 ≤ 50 条、payload ≤ ~128 KB；`split_sentences=1`
- 计费注意：DeepL 按源字符计费——报告里输出每次 run 的源字符总量，便于用户心里有数

### 7.3 OpenAI 兼容 API

- 端点：`POST {base_url}/chat/completions`（base_url 配 `https://api.openai.com/v1`、`https://api.deepseek.com/v1`、Moonshot、SiliconFlow、本地 Ollama `/v1` 等）
- 请求：`{ model, temperature: 0, messages: [system, user] }`；user 消息为“指令 + JSON 字符串数组”
- system prompt 要点（随代码提供，可改）：只翻译不解释；保持数组长度与顺序；每个 `⟦n⟧` 必须且只能出现一次；保留首尾空白；目标语种简体中文；术语以所附术语表为准（取 top N，控 token）；返回纯 JSON 数组，无代码块围栏
- `structured_output = true` 时加 `response_format: {"type":"json_object"}`（提供方不支持时配置关闭，走宽松解析）
- 宽松解析管线：直接 parse → 剥 ```json 围栏 → 截取首个 `[` 到末个 `]` → 按行/按索引正则兜底 → 单条重试 → 标记失败

### 7.4 回退链与限流

- 配置 `chain = ["deepl", "gpt"]`：按序尝试，某 provider 连续失败 N 批（默认 5）触发熔断，本 run 内跳过
- 429：尊重 `Retry-After` / `x-ratelimit-reset`，否则指数退避 + 抖动（上限 30s）
- 456（DeepL 额度尽）/ 401 / 403（鉴权失败）：不重试，直接切下一个 provider 并记录
- 每条 provider 独立令牌桶（rpm/tpm 可配）+ tokio Semaphore 控并发
- 单批超限（413 / context_length_exceeded）：自动二分重试

---

## 8. SQL 数据库设计

默认 SQLite，WAL 模式。DDL：

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS runs (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at    TEXT NOT NULL DEFAULT (datetime('now')),
  finished_at   TEXT,
  src_lang      TEXT NOT NULL DEFAULT 'en_us',
  tgt_lang      TEXT NOT NULL,
  mode          TEXT NOT NULL,              -- zh_cn | inplace
  pack_name     TEXT,
  status        TEXT NOT NULL DEFAULT 'running',  -- running|done|failed|cancelled
  stats_json    TEXT
);

CREATE TABLE IF NOT EXISTS file_states (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  container     TEXT NOT NULL,              -- zip 内路径或相对路径
  kind          TEXT NOT NULL,              -- json | lang | snbt
  encoding      TEXT,
  status        TEXT NOT NULL DEFAULT 'pending',  -- pending|done|failed|skipped
  units_total   INTEGER NOT NULL DEFAULT 0,
  units_done    INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(run_id, container)
);
CREATE INDEX IF NOT EXISTS idx_file_states_status ON file_states(run_id, status);

CREATE TABLE IF NOT EXISTS source_texts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  src_lang      TEXT NOT NULL,
  tgt_lang      TEXT NOT NULL,
  content       TEXT NOT NULL,              -- 原文（未掩码）
  content_sha   TEXT NOT NULL,              -- sha256(src_lang \x1f tgt_lang \x1f content)
  content_len   INTEGER NOT NULL,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(src_lang, tgt_lang, content_sha)
);
CREATE INDEX IF NOT EXISTS idx_source_texts_sha ON source_texts(content_sha);

CREATE TABLE IF NOT EXISTS translations (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id     INTEGER NOT NULL REFERENCES source_texts(id) ON DELETE CASCADE,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  translated    TEXT NOT NULL,              -- 校验通过后的译文
  placeholder_ok INTEGER NOT NULL DEFAULT 1,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(source_id, provider, model)
);
CREATE INDEX IF NOT EXISTS idx_translations_source ON translations(source_id);

CREATE TABLE IF NOT EXISTS glossary_terms (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  term_src      TEXT NOT NULL,
  term_tgt      TEXT NOT NULL,
  note          TEXT,
  case_sensitive INTEGER NOT NULL DEFAULT 0,
  enabled       INTEGER NOT NULL DEFAULT 1,
  UNIQUE(term_src, term_tgt)
);

CREATE TABLE IF NOT EXISTS provider_stats (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  requests      INTEGER NOT NULL DEFAULT 0,
  failures      INTEGER NOT NULL DEFAULT 0,
  retries       INTEGER NOT NULL DEFAULT 0,
  input_chars   INTEGER NOT NULL DEFAULT 0,
  output_chars  INTEGER NOT NULL DEFAULT 0,
  total_ms      INTEGER NOT NULL DEFAULT 0,
  UNIQUE(run_id, provider, model)
);
```

设计要点：

- **缓存键**：`src_lang + tgt_lang + sha256(原文)`，与包、文件路径无关——同一个字符串在 100 个 jar 里出现也只译一次
- **多 provider 共存**：`translations` 按 (source, provider, model) 唯一；查询时先找当前 chain 里的 provider，没有再退而求其次取任意 `placeholder_ok=1` 的行（标记为 reused）
- **术语表变更**：术语后处理应用在缓存之后；改了术语表可 `mctrans cache reapply-glossary` 重算，或 `--no-cache` 全量重跑
- **并发写**：SQLite 单写者 actor（一个 task 独占 `Mutex<Connection>`，其余通过 channel 发请求）+ WAL + busy_timeout + 语句级 SQLITE_BUSY 重试；跨进程用 fs2  advisory 锁 DB 文件，第二进程快速失败并提示
- **迁移**：`PRAGMA user_version` 版本号 + 只增迁移脚本，启动时自动执行
- **完整性**：定期 `PRAGMA integrity_check`；DB 损坏时备份旧库、建新库并以 `--no-cache` 告警运行

---

## 9. 异常处理矩阵

| 类别 | 场景 | 检测 | 策略 | 恢复点 |
|---|---|---|---|---|
| 网络 | 连接中断 / DNS / 超时 | reqwest error kind | connect/read/total 三级超时；指数退避+抖动；切 provider | 单批重试 |
| 限流 | 429 | 状态码 + Retry-After 头 | 按头退避，否则退避；令牌桶降速；熔断计数 | 重试 / 切换 |
| 额度 | DeepL 456 / 401 / 403 | 状态码 | 不重试；标记 provider 不可用；切 fallback | run 级 |
| API 返回异常 | 数组长度不符 / 字段缺失 / 译文为空 | schema 校验 | 宽松解析管线 → 单条重试 → 标记 failed | 条目级 |
| 上下文超限 | 400 context_length_exceeded | 错误体解析 | 二分批次 / 拆条重试 | 批 / 条目级 |
| 编码 | 非 UTF-8 / 无 BOM / GBK | 严格解码探测 | 按 profile 解码；写回原编码；编码记库 | 文件级 |
| 超长 | entry > 预算 | 字符统计 | 安全句子拆分（不切占位符） | 条目级 |
| 特殊字符 | emoji、组合字符、控制符、零宽 | 扫描 | 原样保留；JSON 重新转义；不做 NFC 归一 | 条目级 |
| 并发写 | 两个进程写同一输出包 | fs2 锁（输出目录 + DB） | 快速失败 + 明确提示谁持有 | run 级 |
| 文件原子性 | 中途崩溃 | 临时文件 + rename | 已完成文件保持完整，未完成文件不留残片 | 文件级 |
| 磁盘 | 空间不足 / 只读 | io error | 开跑前预检空间；失败即停，原包不被破坏 | run 级 |
| 输入损坏 | zip crc 错误 / 条目缺失 | zip crate 报错 | 跳过该文件并报告，不中断整体 | 文件级 |
| 取消 | Ctrl-C / GUI 停止 | ctrlc / CancellationToken | 当前批次完成后停；状态落库；可 resume | run 级 |
| DB | SQLITE_BUSY / 损坏 | 错误码 / integrity_check | busy_timeout + 重试；损坏则备份重建 | 语句级 / run 级 |

**jar 写入安全**：新 zip 写到同目录临时文件，全部条目流式拷贝完成后 rename 覆盖；中途失败删除临时文件，原包不动。其他条目原样拷贝（压缩方式、时间戳不变），等价于“归档工具直接添加文件”，不解压、不重打包。

---

## 10. CLI 与前端

### CLI（clap）

```
mctrans scan <pack> [--json]                       # 只分析：可译文件、单元数、预计字符
mctrans translate <pack> -o <out> [选项]            # 翻译
    --mode zh_cn|inplace        --target zh_cn
    --chain deepl,gpt           --concurrency 8
    --force  --dry-run  --no-cache  --resume <run_id>
    --config <path>  --db <path>  --log-level  --json
mctrans serve [--port 8787]                        # 本地 API，供 GUI 复用核心
mctrans cache stats|export|import|clear|reapply-glossary
mctrans glossary import|export|list|add|remove
mctrans config init [--path]                       # 生成带注释的 config.toml
```

### Tauri GUI（ui/，React + TS）

页面：① 选择包/目录 + 目标语言 + 模式；② 翻译源设置（env/keychain 测试连接）；③ 术语表管理；④ 实时进度与日志（Tauri event）；⑤ 结果报告（失败条目一键重试）；⑥ 缓存管理；⑦ 关于/更新。

Tauri command 层只做参数校验 + 调 core + 事件转发，业务全在 `mctrans-core`，CLI 与 GUI 永远一致。

密钥优先级：env（`DEEPL_API_KEY` 等）> 系统 keychain（keyring crate）> config 里的 `api_key`（明文，仅本地调试用，启动时告警）。

---

## 11. 配置说明

配置查找顺序：`--config` > `./mctrans.toml` > `%APPDATA%/mctrans/config.toml`（Linux 走 XDG）。

```toml
[general]
src_lang = "en_us"
mode = "zh_cn"                 # zh_cn: 生成新语言文件；inplace: 原地替换
concurrency = 8
db_path = "auto"               # auto = 用户数据目录
cache = true

[providers.deepl]
type = "deepl"
base_url = "https://api.deepl.com"      # 免费版: https://api-free.deepl.com
api_key_env = "DEEPL_API_KEY"
formality = "default"

[providers.gpt]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DS_API_KEY"
model = "deepseek-chat"
temperature = 0.0
structured_output = true

[fallback]
chain = ["deepl", "gpt"]
circuit_breaker_failures = 5

[placeholder]
sentinel = "⟦{}⟧"
bukkitt_amp_codes = true       # &6 风格开关
custom_patterns = []

[limits]
max_batch_texts = 40
max_batch_chars = 20000
max_entry_chars = 8000
request_timeout_secs = 60
max_retries = 5
```

---

## 12. 一键安装器

- Tauri v2 bundler 产 NSIS 安装包（`mctrans_x.y.z_x64-setup.exe`）：
  - GUI 主程序 + `mctrans.exe` CLI（安装时可选加入 PATH）
  - WebView2 Runtime 检测，缺失时引导安装
  - 开始菜单 / 卸载入口
- 同时发布便携版 `mctrans-portable.zip`（解压即用，数据目录跟随）
- `installer/` 放 NSIS 定制脚本（安装页文案、PATH 选项、数据目录迁移逻辑）
- 发布物附 SHA256；自动更新（Tauri updater + latest.json）列为 v1.1 增强项

---

## 13. 测试策略

| 层 | 内容 |
|---|---|
| 单元 | 占位符规则表驱动用例 ≥ 80 条；随机语料 fuzz（随机串注入占位符，round-trip 断言零丢失） |
| Golden | `samples/` 迷你整合包（含 jar、snbt、lang、GBK 文件）：解析→抽串→回写必须与原文逐字节一致（除目标 key 的 value） |
| Provider | wiremock 模拟：成功 / 429+Retry-After / 500 / 坏 JSON / 数组长度不符 / 413，断言重试、回退、熔断行为 |
| 集成 | 全流程 e2e；中途杀进程后 `--resume`，断言输出与一次跑完完全一致 |
| UI | vitest 关键组件；主流程 Playwright 冒烟一条 |
| CI | GitHub Actions：fmt、clippy -D warnings、test（ubuntu + windows）、tag 构建 release 矩阵 |

---

## 14. Git 与文档

- 本目录即仓库根，当前为空；确认后 `git init`
- `.gitignore`：`target/`、`node_modules/`、`dist/`、`*.db`、`*.log`、运行产物
- Conventional Commits；trunk-based；版本 tag `v*.*.*`
- GitHub 连接器当前未授权：先建本地仓库正常提交；授权后代推 + 配 Actions release 工作流
- 文档：`README.MD`（快速开始、功能、FAQ）+ `docs/ARCHITECTURE.md`、`docs/CONFIG.md`、`docs/USAGE.md`、`docs/PLACEHOLDERS.md`、`docs/INSTALLER.md`，全部中文

---

## 15. 里程碑

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| M0 | 仓库脚手架、CI、README 骨架 | `cargo build` 全绿，CI 过 |
| M1 | 占位符引擎 + 三个结构编辑器 + 编码探测 | golden 测试零 diff；fuzz 通过 |
| M2 | SQLite schema + cache actor + 术语表 | 并发压测无 SQLITE_BUSY 外泄 |
| M3 | DeepL provider + 重试/限流/熔断 | wiremock 全场景通过 |
| M4 | OpenAI 兼容 provider + 宽松解析 + fallback chain | 坏 JSON 场景自动恢复 |
| M5 | 编排管线：扫描→送译→校验→原子提交→报告→resume | e2e + kill 后 resume 输出一致 |
| M6 | CLI 完善 + config | 本文档第 10、11 节功能全覆盖 |
| M7 | Tauri GUI | 主流程可在界面完成 |
| M8 | 安装器 + 文档 | setup.exe 装上即用；文档与实现一致 |
| M9 | 样本包实测 + 修复 + 发布 | 用一个真实整合包跑通并抽查游戏内加载 |

---

## 16. 风险与缓解

| 风险 | 缓解 |
|---|---|
| FTB Quests 版本间 snbt 结构差异 | 结构编辑器只认语法不认 schema；字段白名单可配；golden 测试覆盖多版本样本 |
| 模型改坏占位符 | 多重集校验 + 修复 + 原文兜底；报告显式列出所有 mismatch |
| DeepL 字符计费超预期 | 缓存去重 + 报告显示字符账单；提供 `scan` 预估 |
| GBK 误判 | UTF-8 严格优先、BOM 优先、按文件 profile，误判时可 `--encoding` 覆盖 |
| 大包（10 万+ 条目）耗时 | 并发 + 令牌桶 + 缓存 + resume；进度事件实时可见 |
| Tauri WebView2 老系统缺失 | 安装器检测 + 引导；便携版 zip 兜底 |
| Provider 内容策略拦截个别文本 | 标记 failed + 保留原文；可切换 provider 重试 |

---

## 17. 待确认决策点

| # | 决策 | 建议 | 备选 |
|---|---|---|---|
| D1 | 前端形态 | Tauri v2 一体化（GUI+CLI+安装器一套产出） | CLI + 独立 Vite Web UI（需另做安装器，两套入口） |
| D2 | 数据库 | SQLite 单文件 | 直接上 MySQL/Postgres（部署成本高，v2 再说） |
| D3 | 默认模式 | `zh_cn` 增量新增 | `inplace`（会改原文，回退成本高） |
| D4 | 默认翻译链 | DeepL 优先（格式稳、按量计费）+ OpenAI 兼容兜底 | 只用其一；或倒置顺序 |
| D5 | Git 远程 | 先本地提交，GitHub 授权后代推送 | 现在授权 |

确认 D1–D5（或直接说"按建议来"）后，我从 M0 开始编码。
