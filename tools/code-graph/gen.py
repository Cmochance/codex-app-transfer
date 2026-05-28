#!/usr/bin/env python3
"""Generate a self-contained interactive code dependency graph for the workspace.

Nodes = Cargo workspace crates, edges = internal (path) dependencies, both derived
live from `cargo metadata` so the graph stays accurate as crates/deps change.
Output is a single self-contained `index.html` (D3 v7 + fonts from CDN) deployed to
GitHub Pages by `.github/workflows/code-graph.yml`.

Local run:  python3 tools/code-graph/gen.py --out _site/index.html
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import pathlib
import re
import subprocess


def sh(args: list[str], cwd: str | None = None) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True)


def workspace_root(explicit: str | None) -> str:
    if explicit:
        return os.path.abspath(explicit)
    return sh(["git", "rev-parse", "--show-toplevel"]).strip()


def cargo_metadata(root: str) -> dict:
    return json.loads(sh(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root))


def build_graph(meta: dict, root: str) -> dict:
    ws_ids = set(meta["workspace_members"])
    pkgs = [p for p in meta["packages"] if p["id"] in ws_ids]
    names = {p["name"] for p in pkgs}

    nodes: list[dict] = []
    links: list[dict] = []
    app_version = ""

    for p in pkgs:
        crate_dir = pathlib.Path(p["manifest_path"]).parent
        rs_files = sum(1 for _ in crate_dir.rglob("*.rs"))
        kinds = {k for t in p["targets"] for k in t["kind"]}
        is_bin = "bin" in kinds

        # internal deps: prefer runtime (kind=None) over dev/build when both declared
        internal: dict[str, bool] = {}  # dep name -> dev_only
        for dep in p["dependencies"]:
            name = dep["name"]
            if name not in names or name == p["name"]:
                continue
            dev_only = dep.get("kind") is not None  # None=normal, "dev"/"build"=secondary
            if name not in internal or not dev_only:
                internal[name] = dev_only
        dep_names = sorted(internal)

        role = "app" if is_bin else ("foundation" if not dep_names else "core")
        if is_bin and not app_version:
            app_version = p["version"]

        nodes.append({
            "id": p["name"],
            "version": p["version"],
            "files": rs_files,
            "deps": dep_names,
            "role": role,
            "path": os.path.relpath(crate_dir, root),
            "desc": (p.get("description") or "").strip(),
        })
        for dep_name, dev_only in internal.items():
            links.append({"source": p["name"], "target": dep_name, "dev": dev_only})

    # most-connected crates first (stable, helps the simulation settle the hub centrally)
    nodes.sort(key=lambda n: (-len(n["deps"]), n["id"]))
    return {
        "nodes": nodes,
        "links": links,
        "app_version": app_version,
        "file_total": sum(n["files"] for n in nodes),
    }


def commit_sha(root: str) -> str:
    env = os.environ.get("GITHUB_SHA")
    if env:
        return env[:7]
    try:
        return sh(["git", "rev-parse", "--short", "HEAD"], cwd=root).strip()
    except Exception:
        return "unknown"


def render(graph: dict, commit: str) -> str:
    generated = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    version = graph["app_version"]
    tokens = {
        # escape "</" so a crate description containing "</script>" can't close
        # the inline <script> early
        "__GRAPH_JSON__": json.dumps(
            {"nodes": graph["nodes"], "links": graph["links"]}, ensure_ascii=False
        ).replace("</", "<\\/"),
        "__VERSION__": f"v{version}" if version else "workspace",
        "__GENERATED__": generated,
        "__COMMIT__": commit,
        "__REPO__": "https://github.com/Cmochance/codex-app-transfer",
        "__CRATES__": str(len(graph["nodes"])),
        "__EDGES__": str(len(graph["links"])),
        "__FILES__": str(graph["file_total"]),
    }
    # single-pass substitution: already-injected content (e.g. the JSON, which may
    # contain a literal "__VERSION__" inside a crate description) is never re-scanned
    pattern = re.compile("|".join(re.escape(k) for k in tokens))
    return pattern.sub(lambda m: tokens[m.group(0)], HTML_TEMPLATE)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", default="_site/index.html", help="output HTML path")
    ap.add_argument("--root", default=None, help="workspace root (default: git toplevel)")
    args = ap.parse_args()

    root = workspace_root(args.root)
    graph = build_graph(cargo_metadata(root), root)
    html = render(graph, commit_sha(root))

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(html, encoding="utf-8")
    print(f"wrote {out} — {len(graph['nodes'])} crates, {len(graph['links'])} edges, "
          f"{graph['file_total']} .rs files")


HTML_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Codex App Transfer · Code Graph</title>
<script src="https://cdn.jsdelivr.net/npm/d3@7/dist/d3.min.js"></script>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Syne:wght@600;700;800&family=JetBrains+Mono:wght@400;500;700&display=swap" rel="stylesheet">
<style>
  :root {
    --app: #f0b429; --core: #ff7a5c; --foundation: #2dd4bf; --accent: #5eead4;
    --ink: #e8edf6; --muted: #8b97b0; --faint: #5b6680;
    --glass: rgba(17,23,41,0.55); --hair: rgba(255,255,255,0.09);
    --mono: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    --display: "Syne", -apple-system, sans-serif;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { height: 100%; }
  body {
    font-family: var(--mono); color: var(--ink); overflow: hidden;
    display: flex; flex-direction: column; background: #080b14;
  }
  /* layered gradient-mesh atmosphere (no flat black) */
  body::before {
    content: ""; position: fixed; inset: 0; z-index: -2;
    background:
      radial-gradient(1100px 760px at 16% 8%, rgba(58,86,156,0.34), transparent 58%),
      radial-gradient(900px 680px at 88% 14%, rgba(120,72,164,0.26), transparent 55%),
      radial-gradient(1200px 900px at 62% 102%, rgba(20,120,134,0.26), transparent 60%),
      linear-gradient(168deg, #0a0f1d 0%, #080b14 60%, #060812 100%);
  }
  /* faint dot-grid, vignetted toward the edges */
  body::after {
    content: ""; position: fixed; inset: 0; z-index: -1; pointer-events: none;
    background-image: radial-gradient(rgba(150,180,235,0.07) 1px, transparent 1.4px);
    background-size: 30px 30px;
    -webkit-mask-image: radial-gradient(ellipse 80% 75% at 45% 45%, #000 35%, transparent 88%);
            mask-image: radial-gradient(ellipse 80% 75% at 45% 45%, #000 35%, transparent 88%);
  }
  header {
    display: flex; align-items: center; gap: 16px; flex-wrap: wrap; flex-shrink: 0;
    padding: 16px 26px; background: var(--glass);
    -webkit-backdrop-filter: blur(22px) saturate(150%); backdrop-filter: blur(22px) saturate(150%);
    border-bottom: 1px solid var(--hair);
    animation: drop 0.7s cubic-bezier(.2,.7,.2,1) both;
  }
  header h1 {
    font-family: var(--display); font-weight: 800; font-size: 21px; letter-spacing: -0.02em;
    background: linear-gradient(92deg, #fff 10%, #9fb3dd 90%);
    -webkit-background-clip: text; background-clip: text; -webkit-text-fill-color: transparent;
  }
  header .badge {
    font-size: 11px; padding: 3px 11px; border-radius: 999px; font-weight: 600; letter-spacing: 0.02em;
    color: var(--accent); background: rgba(94,234,212,0.12); border: 1px solid rgba(94,234,212,0.30);
  }
  header .sub { color: var(--muted); font-size: 12px; letter-spacing: 0.04em; text-transform: uppercase; }
  .stats { display: flex; gap: 10px; margin-left: auto; }
  .stat {
    padding: 7px 16px; text-align: center; border-radius: 12px;
    background: rgba(255,255,255,0.04); border: 1px solid var(--hair);
  }
  .stat .num {
    font-family: var(--display); font-size: 18px; font-weight: 700;
    background: linear-gradient(180deg, #fff, #aebbd8);
    -webkit-background-clip: text; background-clip: text; -webkit-text-fill-color: transparent;
  }
  .stat .lbl { font-size: 10px; color: var(--muted); letter-spacing: 0.08em; text-transform: uppercase; }
  .main { flex: 1; display: grid; grid-template-columns: 1fr 340px; min-height: 0; }
  .graph-wrap { position: relative; min-width: 0; overflow: hidden; }
  #graph { width: 100%; height: 100%; display: block; cursor: grab; }
  #graph:active { cursor: grabbing; }
  .node circle.body {
    cursor: pointer;
    stroke: rgba(255,255,255,0.40); stroke-width: 1px;
    filter: drop-shadow(0 0 8px var(--glow)) drop-shadow(0 1px 2px rgba(0,0,0,0.6));
    transition: opacity .25s, filter .25s;
  }
  .node text {
    font-family: var(--mono); font-weight: 500; font-size: 12px; fill: var(--ink);
    paint-order: stroke; stroke: rgba(6,9,18,0.85); stroke-width: 3px; pointer-events: none;
    transition: opacity .25s;
  }
  .node.sel circle.body { filter: drop-shadow(0 0 16px var(--glow)) drop-shadow(0 0 5px var(--glow)); }
  .link { fill: none; stroke: rgba(150,178,235,0.22); stroke-width: 1.4px; transition: opacity .25s, stroke .25s; }
  .link.dev { stroke-dasharray: 3 4; stroke: rgba(150,178,235,0.16); }
  .link.flow { stroke: var(--accent); stroke-width: 1.8px; stroke-dasharray: 5 6; animation: flow .9s linear infinite; }
  @keyframes flow { to { stroke-dashoffset: -22; } }
  @keyframes drop { from { opacity: 0; transform: translateY(-14px); } to { opacity: 1; transform: none; } }
  .hint { position: absolute; top: 16px; left: 20px; font-size: 11px; color: var(--faint); letter-spacing: 0.04em; }
  .legend {
    position: absolute; bottom: 18px; left: 18px; padding: 13px 16px; border-radius: 14px;
    background: var(--glass); -webkit-backdrop-filter: blur(18px); backdrop-filter: blur(18px);
    border: 1px solid var(--hair); font-size: 12px; color: var(--muted);
    display: flex; flex-direction: column; gap: 8px;
  }
  .legend .item { display: flex; align-items: center; gap: 9px; }
  .legend .orb { width: 13px; height: 13px; border-radius: 50%; flex-shrink: 0; box-shadow: 0 0 8px currentColor; }
  .legend .dash { width: 20px; border-top: 2px dashed var(--faint); }
  .panel {
    background: var(--glass); -webkit-backdrop-filter: blur(26px) saturate(150%); backdrop-filter: blur(26px) saturate(150%);
    border-left: 1px solid var(--hair); padding: 26px 24px; overflow-y: auto;
  }
  .panel h3 { font-family: var(--display); font-weight: 700; font-size: 18px; color: var(--ink); letter-spacing: -0.01em; word-break: break-all; }
  .panel .ver { color: var(--muted); font-size: 12px; margin: 6px 0 14px; }
  .role-tag { display: inline-flex; align-items: center; gap: 7px; font-size: 11px; padding: 4px 12px; border-radius: 999px; font-weight: 600; margin-bottom: 14px; }
  .role-tag .orb { width: 9px; height: 9px; border-radius: 50%; box-shadow: 0 0 7px currentColor; }
  .panel .desc { font-size: 13px; color: #c4cee2; line-height: 1.6; margin: 4px 0 14px; }
  .panel .row { font-size: 10px; letter-spacing: 0.10em; text-transform: uppercase; color: var(--faint); margin: 14px 0 7px; }
  .panel .path { font-size: 12px; color: var(--accent); word-break: break-all; }
  .chips { list-style: none; display: flex; flex-wrap: wrap; gap: 7px; }
  .chips li {
    font-size: 12px; padding: 4px 11px; border-radius: 999px; cursor: pointer;
    color: #bcd7ff; background: rgba(94,148,255,0.12); border: 1px solid rgba(94,148,255,0.22); transition: .15s;
  }
  .chips li:hover { background: rgba(94,148,255,0.24); transform: translateY(-1px); }
  .empty { color: var(--foundation); font-size: 13px; }
  footer {
    flex-shrink: 0; text-align: center; padding: 9px; font-size: 11px; color: var(--faint); letter-spacing: 0.03em;
    background: var(--glass); -webkit-backdrop-filter: blur(20px); backdrop-filter: blur(20px); border-top: 1px solid var(--hair);
  }
  footer a { color: var(--accent); text-decoration: none; }
  footer code { color: var(--muted); }
  ::-webkit-scrollbar { width: 9px; }
  ::-webkit-scrollbar-thumb { background: rgba(150,178,235,0.18); border-radius: 6px; }
  @media (max-width: 900px) { .main { grid-template-columns: 1fr; } .panel { display: none; } }
</style>
</head>
<body>
<header>
  <h1>Codex App Transfer</h1>
  <span class="badge">__VERSION__</span>
  <span class="sub">Crate Dependency Graph</span>
  <div class="stats">
    <div class="stat"><div class="num">__CRATES__</div><div class="lbl">crates</div></div>
    <div class="stat"><div class="num">__EDGES__</div><div class="lbl">deps</div></div>
    <div class="stat"><div class="num">__FILES__</div><div class="lbl">.rs files</div></div>
  </div>
</header>
<div class="main">
  <div class="graph-wrap">
    <svg id="graph"></svg>
    <div class="hint">drag to reposition &middot; scroll to zoom &middot; click a crate</div>
    <div class="legend" id="legend"></div>
  </div>
  <aside class="panel" id="panel">
    <h3>Select a crate</h3>
    <p class="desc">Click any orb — or a dependency chip — to inspect its path, version, file count and internal dependencies. The whole graph is derived from <code>cargo metadata</code>, so it never drifts from the code.</p>
  </aside>
</div>
<footer>
  <a href="__REPO__">GitHub Repository</a> &middot; auto-generated from <code>cargo metadata</code> &middot;
  <code>__COMMIT__</code> &middot; __GENERATED__
</footer>
<script>
const GRAPH = __GRAPH_JSON__;
const ROLE = {
  app:        { c: "#f0b429", label: "Application / binary" },
  core:       { c: "#ff7a5c", label: "Core library" },
  foundation: { c: "#2dd4bf", label: "Foundation · no internal deps" },
};
const colorOf = r => (ROLE[r] || { c: "#8b97b0" }).c;

const byId = new Map(GRAPH.nodes.map(n => [n.id, n]));
const radius = n => 12 + Math.sqrt(n.files || 0) * 1.6;
const neighbors = new Map(GRAPH.nodes.map(n => [n.id, new Set([n.id])]));
GRAPH.links.forEach(l => { neighbors.get(l.source).add(l.target); neighbors.get(l.target).add(l.source); });

const svg = d3.select("#graph");
const wrap = svg.node().parentElement;
let width = wrap.clientWidth, height = wrap.clientHeight;
svg.attr("viewBox", [0, 0, width, height]);

const defs = svg.append("defs");
// radial gradient per role -> glossy 3D orb (highlight offset top-left)
Object.entries(ROLE).forEach(([role, { c }]) => {
  const g = defs.append("radialGradient").attr("id", "orb-" + role).attr("cx", "34%").attr("cy", "30%").attr("r", "72%");
  g.append("stop").attr("offset", "0%").attr("stop-color", d3.color(c).brighter(1.5));
  g.append("stop").attr("offset", "42%").attr("stop-color", c);
  g.append("stop").attr("offset", "100%").attr("stop-color", d3.color(c).darker(1.4));
});
defs.append("marker").attr("id", "arrow").attr("viewBox", "0 -5 10 10").attr("refX", 9).attr("refY", 0)
  .attr("markerWidth", 6.5).attr("markerHeight", 6.5).attr("orient", "auto")
  .append("path").attr("d", "M0,-5L10,0L0,5").attr("fill", "rgba(150,178,235,0.5)");

const rootG = svg.append("g");
svg.call(d3.zoom().scaleExtent([0.35, 3]).on("zoom", e => rootG.attr("transform", e.transform)));

const sim = d3.forceSimulation(GRAPH.nodes)
  .force("link", d3.forceLink(GRAPH.links).id(d => d.id).distance(135).strength(0.45))
  .force("charge", d3.forceManyBody().strength(-620))
  .force("center", d3.forceCenter(width / 2, height / 2))
  .force("collide", d3.forceCollide().radius(d => radius(d) + 30));

const link = rootG.append("g").selectAll("line").data(GRAPH.links).join("line")
  .attr("class", d => "link" + (d.dev ? " dev" : "")).attr("marker-end", "url(#arrow)");

const node = rootG.append("g").selectAll("g").data(GRAPH.nodes).join("g").attr("class", "node")
  .style("--glow", d => colorOf(d.role))
  .call(d3.drag()
    .on("start", (e, d) => { if (!e.active) sim.alphaTarget(0.3).restart(); d.fx = d.x; d.fy = d.y; })
    .on("drag", (e, d) => { d.fx = e.x; d.fy = e.y; })
    .on("end", (e, d) => { if (!e.active) sim.alphaTarget(0); d.fx = null; d.fy = null; }));

node.append("circle").attr("class", "body")
  .attr("fill", d => `url(#orb-${ROLE[d.role] ? d.role : "core"})`)
  .attr("r", 0)
  .transition().delay((d, i) => 120 + i * 70).duration(650).ease(d3.easeBackOut.overshoot(1.4))
  .attr("r", radius);
node.append("text").attr("x", d => radius(d) + 7).attr("y", 4)
  .text(d => d.id.replace(/^codex-app-transfer-?/, "") || d.id);

node.on("click", (e, d) => { e.stopPropagation(); select(d.id); });
svg.on("click", () => clearSel());

sim.on("tick", () => {
  link.attr("x1", d => d.source.x).attr("y1", d => d.source.y)
      .attr("x2", d => edge(d, "x")).attr("y2", d => edge(d, "y"));
  node.attr("transform", d => `translate(${d.x},${d.y})`);
});
function edge(d, axis) {
  const dx = d.target.x - d.source.x, dy = d.target.y - d.source.y, len = Math.hypot(dx, dy) || 1;
  const off = radius(d.target) + 5;
  return axis === "x" ? d.target.x - (dx / len) * off : d.target.y - (dy / len) * off;
}

function select(id) {
  const near = neighbors.get(id);
  node.classed("sel", n => n.id === id);
  node.selectAll("circle.body").style("opacity", n => near.has(n.id) ? 1 : 0.12);
  node.selectAll("text").style("opacity", n => near.has(n.id) ? 1 : 0.12);
  link.classed("flow", l => l.source.id === id || l.target.id === id)
      .style("opacity", l => (l.source.id === id || l.target.id === id) ? 1 : 0.05);
  showDetail(byId.get(id));
}
function clearSel() {
  node.classed("sel", false);
  node.selectAll("circle.body").style("opacity", 1);
  node.selectAll("text").style("opacity", 1);
  link.classed("flow", false).style("opacity", 1);
}

function showDetail(d) {
  if (!d) return;
  const c = colorOf(d.role);
  const dependents = GRAPH.links.filter(l => (l.target.id || l.target) === d.id).map(l => (l.source.id || l.source));
  const chips = arr => arr.length
    ? `<ul class="chips">${arr.map(x => `<li data-id="${esc(x)}">${esc(x)}</li>`).join("")}</ul>`
    : `<p class="empty">None</p>`;
  let h = `<h3>${esc(d.id)}</h3>`;
  h += `<p class="ver">v${esc(d.version)} &middot; ${d.files} .rs files</p>`;
  h += `<span class="role-tag" style="background:${c}1f;color:${c}"><span class="orb" style="background:${c}"></span>${(ROLE[d.role] || {}).label || d.role}</span>`;
  if (d.desc) h += `<p class="desc">${esc(d.desc)}</p>`;
  h += `<p class="row">Path</p><p class="path">${esc(d.path)}</p>`;
  h += `<p class="row">Depends on (${d.deps.length})</p>${chips(d.deps)}`;
  h += `<p class="row">Used by (${dependents.length})</p>${chips(dependents)}`;
  const panel = document.getElementById("panel");
  panel.innerHTML = h;
  panel.querySelectorAll(".chips li").forEach(li => li.addEventListener("click", () => select(li.dataset.id)));
}
const esc = s => String(s).replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

const legend = document.getElementById("legend");
Object.values(ROLE).forEach(({ c, label }) =>
  legend.insertAdjacentHTML("beforeend",
    `<div class="item"><span class="orb" style="background:${c};color:${c}"></span>${label}</div>`));
legend.insertAdjacentHTML("beforeend", `<div class="item"><span class="dash"></span>dev / build-only edge</div>`);

addEventListener("resize", () => {
  width = wrap.clientWidth; height = wrap.clientHeight;
  svg.attr("viewBox", [0, 0, width, height]);
  sim.force("center", d3.forceCenter(width / 2, height / 2)).alpha(0.3).restart();
});
</script>
</body>
</html>
"""


if __name__ == "__main__":
    main()
