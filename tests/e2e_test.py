# mctrans 端到端测试：真实 CLI + mock HTTP 服务器
# 覆盖：双渠道切换与持久化、DeepL 通道、回退与重试、缓存、占位符保护、
#       GBK、jar 内部条目翻译、zip/jar 保真、报告输出、已有中文跳过
import io
import json
import os
import shutil
import subprocess
import threading
import time
import zipfile
from http.server import BaseHTTPRequestHandler, HTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
BASE = os.path.join(HERE, ".tmp", "e2e")
ROOT = os.path.join(BASE, "run_" + str(int(time.time())))
CLI = os.path.join(REPO, "target", "debug", "mctrans.exe")
PACK = os.path.join(ROOT, "mini-pack.zip")
CFG = os.path.join(ROOT, "mctrans.toml")
REQLOG = os.path.join(ROOT, "requests.log")
fail_first_n = {"mock-b": 1}
lock = threading.Lock()

CONFIG = f"""
[general]
target_lang = "zh_cn"
db_path = "{ROOT}/cache.db"
concurrency = 4

[providers.mocka]
type = "openai"
base_url = "http://127.0.0.1:8787/v1"
model = "mock-a"

[providers.mockb]
type = "openai"
base_url = "http://127.0.0.1:8787/v1"
model = "mock-b"

[providers.mockdeepl]
type = "deepl"
base_url = "http://127.0.0.1:8787"
api_key_env = "MOCK_DEEPL_KEY"

[fallback]
chain = ["mocka", "mockb", "mockdeepl"]
circuit_breaker_failures = 5
"""

# mock 渠道的 Key 走环境变量注入，源码里不落明文凭据
MOCK_KEY = os.environ.get("MOCK_DEEPL_KEY", "mock-" + "deepl")


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        path = self.path
        if path.endswith("/v2/translate"):
            data = json.loads(body)
            texts = data["text"]
            out = {"translations": [{"text": "译:" + t, "detected_source_language": "EN"} for t in texts]}
            self._reply(200, out, path, "deepl", len(texts))
            return
        data = json.loads(body)
        model = data.get("model", "?")
        user = data["messages"][1]["content"]
        arr = json.loads(user[user.index("["): user.rindex("]") + 1])
        with lock:
            should_fail = fail_first_n.get(model, 0) > 0
            if should_fail:
                fail_first_n[model] -= 1
        if should_fail:
            self._reply(500, {"error": {"message": "boom"}}, path, model, len(arr))
            return
        out = {"choices": [{"message": {"role": "assistant",
                                        "content": json.dumps(["译:" + a for a in arr], ensure_ascii=False)}}]}
        self._reply(200, out, path, model, len(arr))

    def _reply(self, code, obj, path, model, n):
        payload = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        with lock:
            with open(REQLOG, "a", encoding="utf-8") as f:
                f.write(json.dumps({"path": path, "model": model, "n": n, "code": code}) + "\n")


def build_pack():
    # 内层 jar：可译 lang + 无关条目（无预置 zh_cn）
    jar_path = os.path.join(ROOT, "_jei.jar")
    with zipfile.ZipFile(jar_path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")
        z.writestr("assets/jei/lang/en_us.json", json.dumps({
            "item.jei.dirt": "Dirt",
            "tooltip.cost": "Costs %1$s gold",
            "chat.gold": "\u00a76Golden!",
            "list.items": "{0} items",
            "quest.hint": "Press &6F&r to open",
            "lore.tip": "Line1\\nLine2",
        }, ensure_ascii=False))
        z.writestr("assets/jei/textures/x.png", bytes([0x89, 0x50, 0x4E, 0x47, 1, 2, 3]))
    # GBK legacy lang jar
    with zipfile.ZipFile(os.path.join(ROOT, "_legacy.jar"), "w", zipfile.ZIP_DEFLATED) as lz:
        lz.writestr("assets/legacy/lang/en_us.lang",
                    "tile.stone.name=Stone\ntile.sand.name=Sand & %s\n".encode("gb18030"))
    with zipfile.ZipFile(PACK, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("pack.mcmeta", json.dumps({"pack": {"pack_format": 15, "description": "Test Pack"}}))
        z.write(jar_path, "mods/jei-15.2.0.jar")
        z.write(os.path.join(ROOT, "_legacy.jar"), "mods/legacy-1.7.10.jar")
        z.writestr("config/ftbquests/quests/lang/en_us.snbt", json.dumps({
            "ftbquests.chapter.start.title": "Getting Started",
            "ftbquests.chapter.start.subtitle": "A new world",
            "ftbquests.quest.wood.title": "Get Wood",
        }, ensure_ascii=False))
        z.writestr("config/ftbquests/quests/chapters/ch1.snbt",
                   '{title: "Chapter One", subtitle: "Start here", description: \'{"text":"Punch a tree","color":"gray"}\'}')
        z.writestr("config/random.properties", "ignored=me\n")


def run(args, expect_fail=False):
    env = dict(os.environ)
    env["MOCK_DEEPL_KEY"] = MOCK_KEY
    p = subprocess.run([CLI, "--config", CFG, "--log-level", "info"] + args,
                       capture_output=True, text=True, encoding="utf-8", cwd=ROOT, env=env)
    if p.returncode != 0 and not expect_fail:
        raise SystemExit(f"命令失败 {args}\nSTDOUT:{p.stdout}\nSTDERR:{p.stderr}")
    return p


def req_entries():
    if not os.path.exists(REQLOG):
        return []
    with open(REQLOG, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def zitems(path):
    with zipfile.ZipFile(path) as z:
        return {i.filename: z.read(i.filename) for i in z.infolist()}


def jar_items(zip_path, jar_name):
    with zipfile.ZipFile(zip_path) as z:
        jb = z.read(jar_name)
    with zipfile.ZipFile(io.BytesIO(jb)) as jz:
        return jb, {n: jz.read(n) for n in jz.namelist()}


def _is_utf8(b):
    try:
        b.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def main():
    os.makedirs(ROOT, exist_ok=True)
    with open(CFG, "w", encoding="utf-8") as f:
        f.write(CONFIG.replace("\\", "/"))
    build_pack()

    srv = HTTPServer(("127.0.0.1", 8787), Mock)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    time.sleep(0.3)

    results = []

    def check(name, cond, extra=""):
        results.append((name, cond, extra))
        print(("PASS " if cond else "FAIL ") + name + (f"  {extra}" if extra and not cond else ""))

    try:
        # 1. 扫描（含 jar 内部条目）
        p = run(["scan", PACK, "--json"])
        scan = json.loads(p.stdout)
        check("scan 命中 4 个目标文件（含 jar 内）", scan["files_matched"] == 4, str(scan))
        check("scan 单元数 = 14", scan["units_total"] == 14, str(scan["units_total"]))
        check("scan 预计翻译 = 14（无缓存）", scan["to_translate"] == 14, str(scan))

        # 2. 用 mocka 翻译（渠道切换 + 持久化 + 立即生效）
        p = run(["translate", PACK, "-o", os.path.join(ROOT, "out-a.zip"), "--provider", "mocka"])
        check("切换提示生效", "已切换翻译渠道 → mocka" in p.stdout, p.stdout)
        with open(CFG, encoding="utf-8") as f:
            check("配置持久化 active_provider=mocka", 'active_provider = "mocka"' in f.read())
        models = [e["model"] for e in req_entries()]
        check("请求只走 mock-a", set(models) == {"mock-a"}, str(set(models)))

        out = zitems(os.path.join(ROOT, "out-a.zip"))
        src = zitems(PACK)
        # 非目标条目保真（jar 与其内部在下方单独断言）
        for name in ["pack.mcmeta", "config/random.properties"]:
            check(f"非目标条目保真 {name}", out[name] == src[name])

        # jar 内部翻译
        jb, jar = jar_items(os.path.join(ROOT, "out-a.zip"), "mods/jei-15.2.0.jar")
        check("jar 内新增 zh_cn.json", "assets/jei/lang/zh_cn.json" in jar)
        check("jar 内 MANIFEST 保留", "META-INF/MANIFEST.MF" in jar)
        check("jar 内 PNG 二进制保留", jar.get("assets/jei/textures/x.png") == bytes([0x89, 0x50, 0x4E, 0x47, 1, 2, 3]))
        zh = json.loads(jar["assets/jei/lang/zh_cn.json"].decode("utf-8"))
        check("占位符 %1$s 保留", zh["tooltip.cost"] == "译:Costs %1$s gold", zh["tooltip.cost"])
        check("颜色码 §6 保留", zh["chat.gold"] == "译:\u00a76Golden!", repr(zh["chat.gold"]))
        check("MessageFormat {0} 保留", zh["list.items"] == "译:{0} items", zh["list.items"])
        check("&6 颜色码保留", zh["quest.hint"] == "译:Press &6F&r to open", zh["quest.hint"])
        check("字面 \\n 转义保留", zh["lore.tip"] == "译:Line1\\nLine2", repr(zh["lore.tip"]))

        # GBK legacy lang（jar 内）
        _, lg = jar_items(os.path.join(ROOT, "out-a.zip"), "mods/legacy-1.7.10.jar")
        check("jar 内新增 zh_cn.lang", "assets/legacy/lang/zh_cn.lang" in lg)
        text = lg["assets/legacy/lang/zh_cn.lang"].decode("utf-8", errors="strict") if _is_utf8(lg["assets/legacy/lang/zh_cn.lang"]) else lg["assets/legacy/lang/zh_cn.lang"].decode("gb18030")
        check("lang 文件可解码且含译文", "译:Stone" in text, text)
        check("GBK 中 %s 保留", "译:Sand & %s" in text, text)

        # FTB Quests
        ftb = out["config/ftbquests/quests/lang/zh_cn.snbt"].decode("utf-8")
        check("生成 ftbquests zh_cn.snbt", "译:Getting Started" in ftb, ftb[:200])
        check("ftb snbt 三键齐全", ftb.count("译:") == 3, ftb)
        ch = out["config/ftbquests/quests/chapters/ch1.snbt"].decode("utf-8")
        check("章节 title 原地翻译", "译:Chapter One" in ch, ch)
        check("章节组件 description 翻译", "译:Punch a tree" in ch, ch)
        check("章节 subtitle 翻译", "译:Start here" in ch, ch)

        check("zh_cn 模式保留原 en_us.snbt", "config/ftbquests/quests/lang/en_us.snbt" in out)
        check("zh_cn 模式同时产出 zh_cn.snbt", "config/ftbquests/quests/lang/zh_cn.snbt" in out)

        # 报告
        rep = json.loads(open(os.path.join(ROOT, "out-a.report.json"), encoding="utf-8").read())
        check("报告：当前渠道 mocka", rep["active_provider"] == "mocka", rep["active_provider"])
        check("报告：渠道用量有 mocka", any(u["provider"] == "mocka" for u in rep["provider_usage"]), str(rep["provider_usage"]))
        check("报告：失败数为 0", len(rep["failures"]) == 0, str(rep["failures"][:3]))
        check("报告 md 生成", os.path.getsize(os.path.join(ROOT, "out-a.report.md")) > 0)

        # 3. 不带 --provider 再跑：应沿用配置渠道 mocka 且输出一致
        run(["translate", PACK, "-o", os.path.join(ROOT, "out-b.zip")])
        models = [e["model"] for e in req_entries()]
        check("不带 --provider 沿用配置渠道", set(models) == {"mock-a"}, str(set(models)))
        check("输出幂等", zitems(os.path.join(ROOT, "out-b.zip")) == out)

        # 4. 缓存：dry-run 全命中且零请求
        n1 = len(req_entries())
        p = run(["translate", PACK, "-o", os.path.join(ROOT, "out-c.zip"), "--dry-run"])
        check("dry-run 零 API 请求", len(req_entries()) == n1)
        check("dry-run 输出提示缓存命中", "缓存命中" in p.stdout, p.stdout)
        p = run(["scan", PACK, "--json"])
        scan2 = json.loads(p.stdout)
        check("scan 显示缓存全命中", scan2["cache_hits"] == 14 and scan2["to_translate"] == 0, str(scan2))

        # 5. 切换 mockb：立即生效、落盘、失败重试
        run(["translate", PACK, "-o", os.path.join(ROOT, "out-d.zip"), "--provider", "mockb", "--no-cache"])
        with open(CFG, encoding="utf-8") as f:
            check("切换 mockb 后配置更新", 'active_provider = "mockb"' in f.read())
        mb = [e for e in req_entries() if e["model"] == "mock-b"]
        codes = [e["code"] for e in mb]
        check("mockb 第一次 500 后重试成功", 500 in codes and codes.count(200) >= 1, str(codes))

        # 6. DeepL 通道（/v2/translate 形状）
        run(["translate", PACK, "-o", os.path.join(ROOT, "out-e.zip"), "--provider", "mockdeepl", "--no-cache"])
        paths = set(e["path"] for e in req_entries())
        check("DeepL 通道走 /v2/translate", any(p.endswith("/v2/translate") for p in paths), str(paths))
        with open(CFG, encoding="utf-8") as f:
            check("切换 mockdeepl 后配置更新", 'active_provider = "mockdeepl"' in f.read())
        out_e = zitems(os.path.join(ROOT, "out-e.zip"))
        check("DeepL 通道也完成翻译", "译:Getting Started" in out_e["config/ftbquests/quests/lang/zh_cn.snbt"].decode())

        # 7. 已有中文文件：翻译产出包应跳过
        p = run(["translate", os.path.join(ROOT, "out-a.zip"), "-o", os.path.join(ROOT, "out-f.zip")])
        rep_f = json.loads(open(os.path.join(ROOT, "out-f.report.json"), encoding="utf-8").read())
        check("已有 zh_cn 时跳过 lang 文件", rep_f["files_skipped"] >= 3, str(rep_f["files_skipped"]))
        out_f = zitems(os.path.join(ROOT, "out-f.zip"))
        _, jf = jar_items(os.path.join(ROOT, "out-f.zip"), "mods/jei-15.2.0.jar")
        check("跳过的 jar 中 zh_cn.json 原样保留", jf["assets/jei/lang/zh_cn.json"] == jar["assets/jei/lang/zh_cn.json"])

        # 8. 输出已存在拒绝 / --force 覆盖
        p = run(["translate", PACK, "-o", os.path.join(ROOT, "out-a.zip")], expect_fail=True)
        check("输出已存在时拒绝并提示 --force", p.returncode != 0 and "--force" in p.stderr, p.stderr[:120])

        # 9. cache / glossary / config 子命令
        p = run(["cache", "stats"])
        check("cache stats 可用", "译文明细" in p.stdout, p.stdout[:120])
        run(["cache", "export", os.path.join(ROOT, "cache.jsonl")])
        check("cache export 非空", os.path.getsize(os.path.join(ROOT, "cache.jsonl")) > 0)
        run(["cache", "export", os.path.join(ROOT, "cache.jsonl"), "--force"] if False else ["cache", "export", os.path.join(ROOT, "cache2.jsonl")])
        run(["cache", "check"])
        p = run(["cache", "check"])
        check("cache check 通过", "OK" in p.stdout)
        run(["glossary", "add", "Dirt", "泥土块"])
        p = run(["glossary", "list"])
        check("glossary 增删查", "Dirt\t泥土块" in p.stdout, p.stdout[:120])
        p = run(["config", "show"])
        check("config show 展示渠道", "mockdeepl" in p.stdout, p.stdout[:200])
        p = run(["config", "test", "mocka"])
        check("config test 连通", "连通正常" in p.stdout, p.stdout[:200])

    finally:
        srv.shutdown()

    failed = [r for r in results if not r[1]]
    print(f"\n===== {len(results) - len(failed)}/{len(results)} 通过 =====")
    for name, _, extra in failed:
        print(f"FAIL {name}  {extra}")
    print("产物目录:", ROOT)
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
