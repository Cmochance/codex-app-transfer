#!/usr/bin/env python3
"""Provider Credit 倍率漂移检测(QoderWork:官方文档;WorkBuddy:无公开源→仅提示手动)。

用法:
    cargo run -q -p codex-app-transfer-registry --example dump_provider_rates > rates.json
    python3 .github/scripts/check_provider_rate_drift.py rates.json

行为:
- QoderWork:拉官方 https://docs.qoder.com/zh/cli/model.md 的「前沿模型 Credit 消耗倍率」表,
  与 registry `qoder_catalog` 的 credit_rate 逐条对比。有**真实漂移**(倍率变化 / 上游新增模型 /
  我们有倍率但上游没了)→ 打印报告 + 退出码 1(供 workflow 开 issue)。
- WorkBuddy:倍率在客户端下发 product 配置(copilot.tencent.com,需 auth,无公开 GET),CI 无法
  免 auth 自动拉 —— 只打印当前 workbuddy_catalog 倍率 + 手动核对指引,不参与 drift 判定。

**已知刻意分歧**(不算漂移,见下常量):
- `MiniMax-M2.7`(我们保留)↔ 官方文档现列 `MiniMax-M3`(用户决定保留 M2.7;倍率一致即 OK)。
- `Qwen3.6-Flash`:官方「前沿模型」表未单列,倍率取客户端 picker,故「不在文档」属预期、不算漂移。
"""
import json
import re
import sys
import urllib.request

QODER_DOC_URL = "https://docs.qoder.com/zh/cli/model.md"

# 我们的 display_name → 官方文档 model 名(刻意的名称分歧;按此映射对齐后只比倍率)。
QODER_DOCS_ALIASES = {
    "MiniMax-M2.7": "MiniMax-M3",  # 用户决定保留 M2.7 展示名;官方现列 M3(倍率一致)
}
# 我们有倍率但官方「前沿模型」表本就不单列的(客户端 picker 源),不算「消失」漂移。
QODER_CLIENT_SOURCED = {"Qwen3.6-Flash"}


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "codex-app-transfer-rate-drift"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        return resp.read().decode("utf-8")


def parse_qoder_frontier_table(md: str) -> dict:
    """解析「前沿模型」段的 | 模型名称 | 简介 | Credit 消耗倍率 | 表 → {name: rate_str}。"""
    lines = md.splitlines()
    # 定位「前沿模型」标题后的第一张 markdown 表
    start = next((i for i, ln in enumerate(lines) if "前沿模型" in ln), None)
    if start is None:
        raise ValueError("未找到「前沿模型」段(文档结构可能变了)")
    rates = {}
    in_table = False
    for ln in lines[start:]:
        s = ln.strip()
        if s.startswith("|"):
            cells = [c.strip() for c in s.strip("|").split("|")]
            # 跳过表头 + 分隔行(--- / 模型名称)
            if not cells or cells[0] in ("模型名称", "") or set(cells[-1]) <= set("-: "):
                in_table = True
                continue
            name = cells[0]
            rate_cell = cells[-1]
            # 归一化倍率:去 × / 空格 / 反斜杠(\~ 之类),留数字
            m = re.search(r"[0-9]+(?:\.[0-9]+)?", rate_cell)
            if name and m:
                rates[name] = m.group(0)
        elif in_table and not s.startswith("|"):
            break  # 表结束
    if not rates:
        raise ValueError("「前沿模型」表解析为空(格式可能变了)")
    return rates


def norm_rate(r) -> str:
    """'0.60' / '0.6' / 0.6 → 归一化成可比字符串(去尾零)。"""
    if r is None:
        return None
    return f"{float(r):g}"


def check_qoder(ours: list) -> tuple:
    """返回 (drift: bool, error: bool, report_lines: list)。

    `error=True` 表示**检测器本身失效**(官方文档拉不到 / 格式变了解析不出)——必须**单独**
    上报,不能当成「无漂移」静默(否则 docs 改版 / 站点挂了时检测器无限期失效、无人知晓)。
    """
    out = ["## QoderWork(官方文档:docs.qoder.com/zh/cli/model.md)"]
    try:
        docs = parse_qoder_frontier_table(fetch(QODER_DOC_URL))
    except Exception as e:
        out.append(f"- :rotating_light: **官方文档拉取/解析失败,检测器可能已失效**:`{e}`")
        out.append("  —— 这**不是**「无漂移」;需人工核对 docs.qoder.com/zh/cli/model.md 是否改版 / 站点是否可达,并修脚本解析。")
        return (False, True, out)

    drift = False
    docs_matched = set()
    for m in ours:
        name = m["display_name"]
        our_rate = norm_rate(m["credit_rate"])
        if our_rate is None:
            continue  # 无固定倍率(Auto)不参与
        doc_name = QODER_DOCS_ALIASES.get(name, name)
        if doc_name in docs:
            docs_matched.add(doc_name)
            doc_rate = norm_rate(docs[doc_name])
            alias_note = f"(别名对齐 `{doc_name}`)" if doc_name != name else ""
            if our_rate != doc_rate:
                drift = True
                out.append(f"- :x: **倍率漂移** `{name}`{alias_note}:我们 `{our_rate}×` ≠ 官方 `{doc_rate}×`")
            else:
                out.append(f"- :white_check_mark: `{name}`{alias_note} `{our_rate}×`")
        elif name in QODER_CLIENT_SOURCED:
            out.append(f"- :information_source: `{name}` `{our_rate}×`(客户端源,官方前沿表未列,预期)")
        else:
            drift = True
            out.append(f"- :x: **我们有、官方没有** `{name}` `{our_rate}×`(上游重命名/下架?需核对)")

    for doc_name, doc_rate in docs.items():
        if doc_name not in docs_matched:
            drift = True
            out.append(f"- :x: **官方新增/未覆盖** `{doc_name}` `{norm_rate(doc_rate)}×`(考虑加进 qoder_catalog)")
    return (drift, False, out)


def report_workbuddy(ours: list) -> list:
    out = ["", "## WorkBuddy(腾讯 CodeBuddy CN)—— 无公开源,仅手动核对"]
    out.append("- WorkBuddy CN 倍率在**客户端下发的 product 配置**(`copilot.tencent.com`,需 auth,无公开 GET);")
    out.append("  CLI npm 包 `@tencent-ai/codebuddy-code` 是国际版、模型集不同,**不能**代替。")
    out.append("- **手动核对**:从桌面 app 缓存 `~/.workbuddy/local_storage/entry_*.info`(base64+gzip 的 product 配置)")
    out.append("  提 `models[].credits`,与下方当前表对比。")
    out.append("")
    out.append("| model | 当前 credit_rate |")
    out.append("|---|---|")
    for m in ours:
        r = m["credit_rate"]
        out.append(f"| `{m['key']}` ({m['display_name']}) | {r if r else '(无)'} |")
    return out


def main():
    if len(sys.argv) < 2:
        print("usage: check_provider_rate_drift.py <rates.json>", file=sys.stderr)
        sys.exit(2)
    data = json.load(open(sys.argv[1], encoding="utf-8"))
    qoder_drift, qoder_error, qoder_report = check_qoder(data.get("qoder", []))
    wb_report = report_workbuddy(data.get("workbuddy", []))

    report = "\n".join(qoder_report + wb_report)
    print(report)

    # 供 workflow 消费:写 GITHUB_OUTPUT(drift = 检测到倍率漂移;check_error = 检测器失效)+ report 文件。
    # 两者任一为 true 都要开 issue —— check_error 单列,避免「检测器挂了」被当成「无漂移」静默。
    import os
    if out_path := os.environ.get("GITHUB_OUTPUT"):
        with open(out_path, "a", encoding="utf-8") as f:
            f.write(f"drift={'true' if qoder_drift else 'false'}\n")
            f.write(f"check_error={'true' if qoder_error else 'false'}\n")
    with open("rate_drift_report.md", "w", encoding="utf-8") as f:
        f.write(report + "\n")

    # 漂移或检测器失效 → 退出码 1(本地/手动跑时可感知);WorkBuddy 段永不触发退出码
    sys.exit(1 if (qoder_drift or qoder_error) else 0)


if __name__ == "__main__":
    main()
