# mctrans — MC 整合包翻译工具

把 Minecraft 整合包里的英文文本（FTB Quests 任务、Mod 语言文件）批量翻译成简体中文，产出一个结构不变、能直接进游戏的新整合包。

- **Rust 核心 + CLI**：`crates/mctrans-core`（解析、占位符、渠道、缓存、管线）+ `crates/mctrans-cli`
- **双翻译渠道**：DeepL API 与任意 OpenAI 兼容接口（DeepSeek / Moonshot / OpenAI / 本地 Ollama 等），可任选其一，**切换后写入配置、立即生效**
- **占位符保护**：§ 颜色码、`&6`、`%s`、`{0}`、转义序列等全部识别并原样保留，译后再做多重集校验，对不上就丢弃译文、保留原文
- **SQLite 缓存**：内容寻址，同一句话只译一次；跨包、跨 run 复用；断点续跑
- **保真改写**：JSON / SNBT / .lang 只改该改的字符串，缩进、键顺序、注释、其它条目字节不变；jar 内部条目也处理，其它条目二进制原样保留

---

## 当前状态（v0.1）

已可用：

- CLI 全功能：`scan` / `translate` / `cache` / `glossary` / `config`
- 双渠道切换与持久化、回退链、限流/5xx 重试、熔断
- zip 整合包与目录模式；jar 容器内部条目翻译与整包重建
- FTB Quests lang 表（snbt）、章节/任务字段（title/subtitle/description/text）、JSON 文本组件
- Mod 语言文件 `en_us.json`、legacy `*.lang`（UTF-8 / GBK / Big5 探测与保形写回）
- SQLite 缓存、断点续跑、运行报告（JSON + Markdown）
- 62 个单元测试 + 49 项端到端测试（内置 mock HTTP 服务器，不依赖真实 API）

还没做（排在 v0.2）：

- Tauri 图形界面与 NSIS 一键安装器（当前是 CLI 版，发布前需自行打包）
- 超长文本的安全句子拆分（超限条目目前整条跳过并在报告里列出）
- 断点续跑的语义还比较粗：靠缓存 + 文件状态表实现，粒度是文件级

---

## 构建

需要 Rust 1.75+（Windows 上 `rusqlite` 需要 C 编译器，MSVC Build Tools 或 MinGW 均可）。

```bash
git clone <repo> && cd mctrans
cargo build --release          # 产物：target/release/mctrans.exe
cargo test --workspace         # 单元测试
python tests/e2e_test.py       # 端到端测试（需先构建 debug 版）
```

Windows 上如果 `cargo build` 报 `cl.exe` 找不到，先在「x64 Native Tools Command Prompt」里执行，或者把 MSVC 的 `VC/Tools/MSVC/<版本>/bin/Hostx64/x64` 加进 PATH。

---

## 快速开始

1. 生成配置：

   ```bash
   mctrans config init          # 在当前目录写一份 mctrans.toml（带注释的模板）
   ```

2. 设置 API Key（**不要把 Key 写进配置文件**）：

   ```powershell
   $env:DEEPL_API_KEY = "你的 DeepL Key"          # DeepL 免费版用 api-free.deepl.com
   $env:DS_API_KEY    = "你的 DeepSeek Key"       # 任何 OpenAI 兼容服务
   ```

3. 先看看包里有什么能被翻：

   ```bash
   mctrans scan MyPack.zip
   ```

4. 翻译：

   ```bash
   mctrans translate MyPack.zip -o MyPack-zh_cn.zip
   ```

产出 `MyPack-zh_cn.zip`：原包内每个 `en_us.json` 旁边多一个 `zh_cn.json`，FTB Quests 多一份 `zh_cn.snbt`，章节文件里的英文被就地替换成中文。游戏语言设为简体中文即可加载。

---

## 渠道切换

两种方式，效果一样（写入配置 + 立即生效）：

```bash
mctrans config use deepl        # 切到 DeepL
mctrans config use gpt          # 切到 OpenAI 兼容通道
mctrans translate MyPack.zip -o out.zip --provider gpt   # 顺手切换并翻译
```

切换结果写在配置的 `general.active_provider`，之后所有 run 默认用它。翻译过程中如果这个渠道失败（限流、5xx、超时、额度用尽），会按 `fallback.chain` 依次回退：

```toml
[fallback]
chain = ["deepl", "gpt"]
circuit_breaker_failures = 5      # 同一渠道连续失败 5 批后，本 run 内跳过它
```

`mctrans config show` 看当前渠道与 Key 状态（Key 只显示首尾几位），`mctrans config test [渠道]` 发一次 Ping 验证连通性。

---

## 常用命令

```
mctrans scan <pack> [--json]                 只分析：命中文件数、单元数、缓存命中、预计翻译量
mctrans translate <pack> [-o <out>]          翻译
    --provider <名称>   切换渠道并持久化
    --mode zh_cn|inplace   zh_cn=新增中文语言文件；inplace=原地替换原文件
    --dry-run         只统计不调用 API
    --no-cache        忽略缓存强制重译
    --resume          跳过上次已完成的文件
    --force           覆盖已存在的输出
    --concurrency N   并发批次数（默认 8）
mctrans cache stats|export <f>|import <f>|clear [--scope all]|check
mctrans glossary list|add <src> <tgt>|remove <src> <tgt>|import <f>|export <f>
mctrans config init|show|use <渠道>|test [渠道]
```

每次翻译会在输出旁边生成 `<out>.report.json` 与 `<out>.report.md`：文件数、单元数、缓存命中、占位符保护失败数、失败条目明细、各渠道的批次与字符数（DeepL 按字符计费，方便对账）。

---

## 配置

配置文件查找顺序：`--config` > 当前目录 `mctrans.toml` > `%APPDATA%/mctrans/mctrans.toml`。

```toml
[general]
src_lang = "en_us"
target_lang = "zh_cn"
mode = "zh_cn"
concurrency = 8
db_path = "auto"          # auto = 用户数据目录
cache = true

[providers.deepl]
type = "deepl"
base_url = "https://api.deepl.com"     # 免费版：https://api-free.deepl.com
api_key_env = "DEEPL_API_KEY"

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
```

渠道名是 `[providers.<名字>]` 里的 `<名字>`，用它来 `--provider`、`config use`。

不带 Key 也能用：OpenAI 兼容渠道允许不配 Key（本地 Ollama 默认就是这种）：

```toml
[providers.local]
type = "openai"
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:7b"
```

完整字段说明见 [docs/CONFIG.md](docs/CONFIG.md)，目标文件匹配规则见 [docs/USAGE.md](docs/USAGE.md)。

---

## 占位符保护

翻译前会把游戏内占位符换成唯一标记（默认 `⟦0⟧`），翻译后换回来并校验：

| 类型 | 例子 |
|---|---|
| MC 颜色码 | `§6`、`§x§7§f§0§0§0§0` |
| Bukkit 颜色码 | `&6`、`&l` |
| printf | `%s`、`%1$d`、`%.2f`、`%%` |
| MessageFormat | `{0}`、`{1,number}` |
| 转义序列 | `\n`、`\t`、`\\` |
| 内联标签 | `<br>`、`&amp;` |

校验规则：每个标记必须原样出现且只出现一次；译文中不得出现原文没有的占位符；首尾空白按原文补齐。任何一项不满足就丢弃这条译文、保留原文，并在报告里列出——不会出现「翻完之后颜色码丢了、%s 变 %d」这种把游戏文本搞坏的情况。

自定义占位符：

```toml
[placeholder]
sentinel = "⟦{}⟧"
amp_codes = true
strict_new = true
custom_patterns = ["#\\[quest\\]#"]
```

细节见 [docs/PLACEHOLDERS.md](docs/PLACEHOLDERS.md)。

---

## 目录结构

```
crates/mctrans-core/     核心库
  src/placeholder/       占位符识别、掩码、校验、修复
  src/text/              JSON / SNBT / .lang 保形改写，编码探测，文本组件
  src/providers/         DeepL、OpenAI 兼容、回退链、限流、熔断
  src/cache/             SQLite schema 与仓储
  src/pack/              zip/jar 读写、容器重建、目标规则
  src/pipeline/          编排、断点续跑、报告
crates/mctrans-cli/      命令行入口
docs/                    规划与说明文档
tests/e2e_test.py        端到端测试（自带 mock HTTP 服务器）
```

---

## 开发

```bash
cargo fmt && cargo clippy --workspace -- -D warnings
cargo test --workspace
```

单元测试覆盖占位符规则（含随机语料 fuzz）、结构编辑器保真、编码探测、缓存、渠道重试与熔断。端到端测试用本地 mock 服务器模拟 DeepL 与 OpenAI 兼容接口，覆盖渠道切换、持久化、回退重试、缓存命中、jar 内部条目、GBK、占位符保留、报告输出。

---

## 已知限制

- 翻译内容里本来就有中文的条目会被跳过（避免二次翻译）
- 单条超过 `max_entry_chars`（默认 8000）会整条跳过并在报告里列出，暂不支持自动拆分
- `inplace` 模式只在输出包上生效，不会改动输入包
- 目前只处理一��嵌套（jar 里的文件），jar 里的 jar 不再往里递归
- FTB Quests 的字段白名单默认是 `title/subtitle/description/text`，其它版本的字段需要在配置里加

---

## 许可

MIT
