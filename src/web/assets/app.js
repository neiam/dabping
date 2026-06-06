"use strict";

const RANGES = [
  ["3h", "Last 3 Hours"],
  ["30h", "Last 30 Hours"],
  ["10d", "Last 10 Days"],
  ["360d", "Last 360 Days"],
];
const THEMES = ["system", "afterdark", "her", "forest", "sky", "clays", "stones", "lofi", "black"];
// loss buckets: [max loss %, label]; colors resolved from the theme
const LOSS_BUCKETS = [
  [0, "0%"],
  [5, "≤5%"],
  [15, "≤15%"],
  [40, "≤40%"],
  [100, ">40%"],
];

// shared daisyUI/Tailwind class strings for the view builders (kept as
// literals in this file so the Tailwind content scanner picks them up)
const CARD = "card bg-base-200 border border-base-300 p-5 mb-4";
const GRID_CARD = "card bg-base-200 border border-base-300 p-5";
const GRID = "grid gap-4 grid-cols-[repeat(auto-fill,minmax(360px,1fr))]";
const CARD_H3 = "text-[13px] font-bold text-base-content/80 mb-2";
const HEAD = "mb-4";
const H1 = "m-0 text-[19px] font-bold tracking-wider";
const SUB = "text-xs text-base-content/60 mt-1";
const ACCENT_B = "text-accent font-normal";
const LEGEND = "flex flex-wrap gap-3 text-[11px] text-base-content/70 mt-2 mb-4";
const CHIP = "inline-block w-3.5 h-1 rounded-sm align-[2px] mr-1";

let TREE = [];
let META = { step: 300, pings: 20 };
// agent overlays the user has hidden (clickable legend chips); shared
// across targets and persisted
const hiddenAgents = new Set(JSON.parse(localStorage.getItem("dabping:agents-off") || "[]"));
// y-axis scale: "linear" (master-smoke scaled, overlays may clip) or "log"
// (also fits the agent overlays' medians); navbar button toggles it
let YSCALE = localStorage.getItem("dabping:yscale") === "log" ? "log" : "linear";
// every canvas currently in the DOM, with what it shows
const charts = new Set();
const refreshTimers = new Map();

/* ---------- theme picker ---------- */

// fa-display-style "System" icon for the theme menu
const SYSTEM_ICON =
  '<svg class="icon" viewBox="0 0 576 512" aria-hidden="true"><path d="M64 32h448c35 0 64 29 64 64v256c0 35-29 64-64 64H355l9 48h52c13 0 24 11 24 24s-11 24-24 24H160c-13 0-24-11-24-24s11-24 24-24h52l9-48H64c-35 0-64-29-64-64V96c0-35 29-64 64-64zm0 64v224h448V96H64z"/></svg>';

function initThemePicker() {
  const dd = document.getElementById("theme-dd");
  const ul = document.getElementById("theme-list");
  const current = () => localStorage.getItem("dabping:theme") || "system";
  const render = () => {
    ul.innerHTML = "";
    for (const t of THEMES) {
      const li = document.createElement("li");
      const b = document.createElement("button");
      b.className = (t === current() ? "active " : "") + "flex justify-between";
      if (t === "system") {
        b.innerHTML = `<span class="flex items-center gap-2">${SYSTEM_ICON} System</span>`;
      } else {
        // swatch scoped to the theme it represents (DMS pattern)
        b.innerHTML = `<span class="flex items-center gap-2 capitalize">${t}</span><span class="swatch" data-theme="${t}"></span>`;
      }
      b.onclick = () => {
        window.dabpingSetTheme(t);
        dd.open = false;
        render();
        redrawAll(); // graph colors come from the theme
      };
      li.appendChild(b);
      ul.appendChild(li);
    }
  };
  render();
  document.addEventListener("click", (e) => {
    if (!dd.contains(e.target)) dd.open = false;
  });
}

/* ---------- y-scale toggle ---------- */

function initYScale() {
  const btn = document.getElementById("yscale-btn");
  const paint = () => { btn.textContent = YSCALE === "log" ? "log" : "lin"; };
  paint();
  btn.onclick = () => {
    YSCALE = YSCALE === "log" ? "linear" : "log";
    localStorage.setItem("dabping:yscale", YSCALE);
    paint();
    redrawAll();
    // compare canvases aren't in the charts set; rebuild that view instead
    if (location.hash.startsWith("#/cmp/")) route();
  };
}

/* ---------- colors ---------- */

function themeColors() {
  const cs = getComputedStyle(document.documentElement);
  const v = (name) => `oklch(${cs.getPropertyValue(name).trim()})`;
  const va = (name, a) => `oklch(${cs.getPropertyValue(name).trim()} / ${a})`;
  return {
    text: v("--bc"),
    faint: va("--bc", 0.55),
    grid: va("--bc", 0.12),
    smoke: va("--bc", 0.16),
    accent: v("--a"),
    loss: [v("--su"), v("--wa"), "oklch(75% 0.17 55)", v("--er"), "oklch(45% 0.16 25)"],
    unknown: va("--bc", 0.35),
  };
}

function lossColor(colors, lossPct) {
  if (!Number.isFinite(lossPct)) return colors.unknown;
  for (let i = 0; i < LOSS_BUCKETS.length; i++) {
    if (lossPct <= LOSS_BUCKETS[i][0]) return colors.loss[i];
  }
  return colors.loss[LOSS_BUCKETS.length - 1];
}

/* ---------- formatting ---------- */

function fmtMs(secs) {
  if (secs == null || !Number.isFinite(secs)) return "–";
  const ms = secs * 1000;
  return (ms < 10 ? ms.toFixed(2) : ms < 100 ? ms.toFixed(1) : ms.toFixed(0)) + "ms";
}

function fmtTime(ts, spanSecs) {
  const d = new Date(ts * 1000);
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  if (spanSecs <= 36 * 3600) return hm;
  const md = `${d.getMonth() + 1}/${d.getDate()}`;
  return spanSecs <= 14 * 86400 ? `${md} ${hm}` : md;
}

function fmtFull(ts) {
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function niceCeil(v) {
  if (!Number.isFinite(v) || v <= 0) return 1;
  const mag = Math.pow(10, Math.floor(Math.log10(v)));
  for (const m of [1, 2, 5, 10]) if (v <= m * mag) return m * mag;
  return 10 * mag;
}

/* ---------- smoke rendering ---------- */

function drawSmoke(canvas, fetched, opts = {}) {
  const colors = themeColors();
  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth, H = canvas.clientHeight;
  canvas.width = W * dpr;
  canvas.height = H * dpr;
  const ctx = canvas.getContext("2d");
  ctx.scale(dpr, dpr);
  ctx.clearRect(0, 0, W, H);

  const mini = !!opts.mini;
  const M = { l: mini ? 42 : 50, r: 8, t: 8, b: mini ? 18 : 22 };
  const pts = fetched.points || [];
  if (!pts.length) return;
  const period = fetched.period;
  const x0 = pts[0].ts, x1 = pts[pts.length - 1].ts + period;
  const span = x1 - x0;

  // y scale from the widest smoke; log mode also makes room for the agent
  // overlays, whose medians can sit far above the master's smoke
  const log = YSCALE === "log";
  let top = 0, lopos = Infinity;
  const see = (ms) => { top = Math.max(top, ms); if (ms > 0) lopos = Math.min(lopos, ms); };
  for (const p of pts)
    for (const v of p.pings) if (Number.isFinite(v)) see(v * 1000);
  if (log)
    for (const s of opts.agents || [])
      for (const p of s.fetched.points || [])
        if (Number.isFinite(p.median)) see(p.median * 1000);
  const ymax = niceCeil(top * 1.05);

  const X = (t) => M.l + ((t - x0) / span) * (W - M.l - M.r);
  let Y, ticks;
  if (log) {
    if (!Number.isFinite(lopos)) lopos = ymax / 100;
    // bottom of the axis: smallest value floored to a power of 10, but at
    // least one decade below the top so the scale never degenerates
    const lo = Math.pow(10, Math.min(Math.floor(Math.log10(lopos)), Math.ceil(Math.log10(ymax)) - 1));
    const lgLo = Math.log10(lo), lgHi = Math.log10(ymax);
    Y = (ms) => M.t + (1 - (Math.log10(Math.max(ms, lo)) - lgLo) / (lgHi - lgLo)) * (H - M.t - M.b);
    // decade gridlines, with 2×/5× subdivisions when the span is short
    const mults = lgHi - lgLo > (mini ? 2 : 3) ? [1] : mini ? [1, 5] : [1, 2, 5];
    ticks = [];
    for (let d = Math.floor(lgLo); d <= Math.ceil(lgHi); d++)
      for (const m of mults) {
        const v = m * Math.pow(10, d);
        if (v >= lo * 0.999 && v <= ymax * 1.001) ticks.push(v);
      }
  } else {
    Y = (ms) => M.t + (1 - ms / ymax) * (H - M.t - M.b);
    const yticks = mini ? 2 : 4;
    ticks = Array.from({ length: yticks + 1 }, (_, i) => (ymax / yticks) * i);
  }

  // grid + y labels
  ctx.font = `10px "B612 Mono", monospace`;
  ctx.fillStyle = colors.faint;
  ctx.strokeStyle = colors.grid;
  ctx.lineWidth = 1;
  for (const ms of ticks) {
    const y = Y(ms);
    ctx.beginPath();
    ctx.moveTo(M.l, y);
    ctx.lineTo(W - M.r, y);
    ctx.stroke();
    ctx.textAlign = "right";
    ctx.fillText(ms >= 1000 ? (ms / 1000) + "s" : ms + "ms", M.l - 5, y + 3);
  }
  // x labels
  const xticks = mini ? 3 : Math.min(6, Math.floor(W / 110));
  ctx.textAlign = "center";
  for (let i = 0; i <= xticks; i++) {
    const t = x0 + (span / xticks) * i;
    ctx.fillText(fmtTime(t, span), X(t), H - 6);
  }

  if (opts.axesOnly) {
    // compare view: scale/axes from synthetic bounds, series drawn as overlays
    canvas._chart = { pts, period, x0, span, X, Y, M, W, H };
    return;
  }

  // smoke: stacked translucent quantile bands
  smokeBands(ctx, pts, period, X, Y, colors.smoke);

  // median line, segment-colored by loss
  ctx.lineWidth = mini ? 1.5 : 2;
  let prev = null;
  for (let i = 0; i < pts.length; i++) {
    const p = pts[i];
    const med = Number.isFinite(p.median) ? p.median * 1000 : null;
    const x = X(p.ts + period / 2);
    if (med != null && prev) {
      ctx.strokeStyle = lossColor(colors, p.loss);
      ctx.beginPath();
      ctx.moveTo(prev.x, Y(prev.med));
      ctx.lineTo(x, Y(med));
      ctx.stroke();
    } else if (med != null && (i + 1 >= pts.length || !Number.isFinite(pts[i + 1].median))) {
      // isolated point — make it visible
      ctx.fillStyle = lossColor(colors, p.loss);
      ctx.fillRect(x - 1.5, Y(med) - 1.5, 3, 3);
    }
    prev = med != null ? { x, med } : null;
  }

  canvas._chart = { pts, period, x0, span, X, Y, M, W, H };
}

// stacked translucent quantile bands between v[k] and v[len-1-k]; shared
// by the master smoke and the per-agent overlays (tinted via fill)
function smokeBands(ctx, pts, period, X, Y, fill) {
  // per-point sorted valid pings (ms)
  const valid = pts.map((p) => p.pings.filter((v) => Number.isFinite(v)).map((v) => v * 1000));
  const maxValid = Math.max(0, ...valid.map((v) => v.length));
  ctx.fillStyle = fill;
  for (let k = 0; k < Math.floor(maxValid / 2) + 1; k++) {
    let run = [];
    const flush = () => {
      if (run.length >= 2) {
        ctx.beginPath();
        ctx.moveTo(run[0].x, Y(run[0].lo));
        for (const s of run) ctx.lineTo(s.x, Y(s.lo));
        for (let j = run.length - 1; j >= 0; j--) ctx.lineTo(run[j].x, Y(run[j].hi));
        ctx.closePath();
        ctx.fill();
      } else if (run.length === 1) {
        const s = run[0];
        ctx.fillRect(s.x - 1, Y(s.hi), 2, Math.max(1, Y(s.lo) - Y(s.hi)));
      }
      run = [];
    };
    for (let i = 0; i < pts.length; i++) {
      const v = valid[i];
      if (v.length >= 2 * k + 1 && v.length > 0) {
        run.push({ x: X(pts[i].ts + period / 2), lo: v[k], hi: v[v.length - 1 - k] });
      } else {
        flush();
      }
    }
    flush();
  }
}

// distinguishable overlay colors for agent series (8 vantage regions)
const AGENT_COLORS = ["#60a5fa", "#f472b6", "#fbbf24", "#34d399", "#c084fc", "#22d3ee", "#fb7185", "#a3e635"];

function agentColor(i) {
  return AGENT_COLORS[i % AGENT_COLORS.length];
}

// an agent's series on the master graph's scale: tinted smoke bands plus a
// solid median polyline; smoke:false (compare view) = dashed median only
function drawAgentSeries(canvas, fetched, color, opts = {}) {
  const ch = canvas._chart;
  if (!ch) return;
  const ctx = canvas.getContext("2d");
  const dpr = window.devicePixelRatio || 1;
  ctx.save();
  // absolute transform: drawSmoke leaves the context dpr-scaled, so a
  // relative scale() here would double it on hidpi and draw off-canvas
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  // agent values can exceed the master's linear scale — keep the spill
  // inside the plot area instead of painting over labels
  ctx.beginPath();
  ctx.rect(ch.M.l, ch.M.t, ch.W - ch.M.l - ch.M.r, ch.H - ch.M.t - ch.M.b);
  ctx.clip();
  if (opts.smoke) smokeBands(ctx, fetched.points || [], fetched.period, ch.X, ch.Y, color + "1a");
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.2;
  if (!opts.smoke) ctx.setLineDash([4, 3]);
  let prev = null;
  for (const p of fetched.points) {
    const med = Number.isFinite(p.median) ? p.median * 1000 : null;
    const x = ch.X(p.ts + fetched.period / 2);
    if (med != null && prev) {
      ctx.beginPath();
      ctx.moveTo(prev.x, ch.Y(prev.med));
      ctx.lineTo(x, ch.Y(med));
      ctx.stroke();
    }
    prev = med != null ? { x, med } : null;
  }
  ctx.restore();
}

function drawEmpty(wrap, msg) {
  let note = wrap.querySelector(".empty-note");
  if (!note) {
    note = document.createElement("div");
    note.className = "empty-note absolute inset-0 flex items-center justify-center text-xs text-base-content/45";
    wrap.appendChild(note);
  }
  note.textContent = msg;
}

/* ---------- data loading ---------- */

async function loadGraph(canvas) {
  const { path, range } = canvas.dataset;
  const points = Math.min(400, Math.max(50, Math.floor(canvas.clientWidth / 2)));
  // drag-to-zoom overrides the named range with an explicit window
  const win = canvas._zoom ? `from=${canvas._zoom.from}&to=${canvas._zoom.to}` : `range=${range}`;
  try {
    const r = await fetch(`/api/data/${path}?${win}&points=${points}`);
    if (!r.ok) {
      const e = await r.json().catch(() => ({}));
      drawEmpty(canvas.parentElement, e.error?.includes("no data") ? "no data yet" : (e.error || "error"));
      return;
    }
    canvas.parentElement.querySelector(".empty-note")?.remove();
    const fetched = await r.json();
    canvas._fetched = fetched;
    // per-agent median lines (series live under "path@agent") are fetched
    // BEFORE the smoke draws: log mode scales the y-axis to fit them.
    // Colors index the full agent list so they stay stable when some are
    // toggled off, and fetches are kept for redrawAll (theme/resize).
    const agents = (canvas.dataset.agents || "").split(",").filter(Boolean);
    canvas._agents = (await Promise.all(agents.map(async (a, i) => {
      if (hiddenAgents.has(a)) return null;
      try {
        const ar = await fetch(`/api/data/${path}@${a}?${win}&points=${points}`);
        return ar.ok ? { fetched: await ar.json(), color: agentColor(i) } : null;
      } catch { return null; /* agent series may not exist yet */ }
    }))).filter(Boolean);
    drawSmoke(canvas, fetched, { mini: canvas.classList.contains("mini"), agents: canvas._agents });
    for (const a of canvas._agents) drawAgentSeries(canvas, a.fetched, a.color, { smoke: true });
  } catch {
    drawEmpty(canvas.parentElement, "fetch failed");
  }
}

function scheduleRefresh(canvas, delay = 1200) {
  const key = canvas.dataset.path + "|" + canvas.dataset.range;
  if (refreshTimers.has(key)) return;
  refreshTimers.set(key, setTimeout(() => {
    refreshTimers.delete(key);
    if (canvas.isConnected) loadGraph(canvas);
  }, delay));
}

function redrawAll() {
  for (const c of [...charts]) {
    if (!c.isConnected) { charts.delete(c); continue; }
    if (c._fetched) {
      drawSmoke(c, c._fetched, { mini: c.classList.contains("mini"), agents: c._agents || [] });
      for (const a of c._agents || []) drawAgentSeries(c, a.fetched, a.color, { smoke: true });
    }
  }
}

/* ---------- tree / routing ---------- */

function nodeLabel(n) {
  return n.menu || n.title || n.name;
}

function findNode(path) {
  let nodes = TREE, found = null;
  for (const part of path.split("/")) {
    found = nodes.find((n) => n.name === part);
    if (!found) return null;
    nodes = found.children;
  }
  return found;
}

function leavesUnder(node, acc = []) {
  if (node.host) acc.push(node);
  for (const c of node.children) leavesUnder(c, acc);
  return acc;
}

/* ---------- drag-to-zoom ---------- */

function initZoom() {
  let sel = null; // {canvas, x0, box}
  document.addEventListener("mousedown", (e) => {
    const c = e.target;
    if (!(c instanceof HTMLCanvasElement) || !c._chart || !c.dataset.path || c.classList.contains("mini")) return;
    const rect = c.getBoundingClientRect();
    const box = document.createElement("div");
    box.style.cssText =
      "position:absolute;top:0;bottom:0;background:oklch(70% 0.1 250/.25);pointer-events:none";
    c.parentElement.appendChild(box);
    sel = { canvas: c, rect, x0: e.clientX - rect.left, box };
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!sel) return;
    const x1 = e.clientX - sel.rect.left;
    sel.box.style.left = Math.min(sel.x0, x1) + "px";
    sel.box.style.width = Math.abs(x1 - sel.x0) + "px";
  });
  document.addEventListener("mouseup", (e) => {
    if (!sel) return;
    const { canvas, rect, x0, box } = sel;
    sel = null;
    box.remove();
    const x1 = e.clientX - rect.left;
    if (Math.abs(x1 - x0) < 12) return; // click, not a drag
    const ch = canvas._chart;
    const toTs = (px) =>
      Math.round(ch.x0 + ((px - ch.M.l) / (ch.W - ch.M.l - ch.M.r)) * ch.span);
    canvas._zoom = { from: toTs(Math.min(x0, x1)), to: toTs(Math.max(x0, x1)) };
    loadGraph(canvas);
  });
  // double-click resets to the card's named range
  document.addEventListener("dblclick", (e) => {
    const c = e.target;
    if (c instanceof HTMLCanvasElement && c._zoom) {
      delete c._zoom;
      loadGraph(c);
    }
  });
}

function renderTree() {
  const el = document.getElementById("tree");
  const build = (nodes) => {
    const ul = document.createElement("ul");
    for (const n of nodes) {
      const li = document.createElement("li");
      if (n.host) {
        const a = document.createElement("a");
        a.href = `#/t/${n.path}`;
        a.dataset.path = n.path;
        a.textContent = nodeLabel(n);
        li.appendChild(a);
      } else {
        const a = document.createElement("a");
        a.href = `#/s/${n.path}`;
        a.className = "uppercase text-xs font-bold tracking-wider text-base-content/85";
        a.textContent = nodeLabel(n);
        li.appendChild(a);
        li.appendChild(build(n.children));
      }
      ul.appendChild(li);
    }
    return ul;
  };
  el.innerHTML = "";
  // daisyUI menu: nested <ul>s indent, leaf links get hover/.active styling
  const root = build(TREE);
  root.className = "menu menu-sm p-0";
  const extra = document.createElement("li");
  extra.className = "mt-4";
  extra.innerHTML = `<a href="#/charts">⚡ charts</a>`;
  root.appendChild(extra);
  if (META.status) {
    // public statuspage — plain page outside the SPA router
    const st = document.createElement("li");
    st.innerHTML = `<a href="/status">✓ status</a>`;
    root.appendChild(st);
  }
  el.appendChild(root);
}

function markActive(path) {
  for (const a of document.querySelectorAll("#tree a[data-path]"))
    a.classList.toggle("active", a.dataset.path === path);
}

function route() {
  const h = decodeURIComponent(location.hash.slice(1));
  charts.clear();
  if (h.startsWith("/t/")) return viewDetail(h.slice(3));
  if (h.startsWith("/s/")) return viewSection(h.slice(3));
  if (h.startsWith("/cmp/")) return viewCompare(h.slice(5));
  if (h === "/charts") return viewCharts();
  if (TREE.length) {
    const first = TREE[0];
    return first.host ? viewDetail(first.path) : viewSection(first.path);
  }
}

/* ---------- views ---------- */

function legendHtml() {
  const colors = themeColors();
  const chips = LOSS_BUCKETS.map(
    ([, label], i) =>
      `<span><span class="${CHIP}" style="background:${colors.loss[i]}"></span>loss ${label}</span>`
  ).join("");
  return `<div class="${LEGEND}"><span>median ─ colored by loss</span>${chips}<span><span class="${CHIP}" style="background:${colors.smoke}"></span>smoke = round distribution</span></div>`;
}

function viewDetail(path) {
  markActive(path);
  const node = findNode(path);
  const view = document.getElementById("view");
  const title = node ? (node.title || nodeLabel(node)) : path;
  const host = node?.host ? `<b class="${ACCENT_B}">${node.host}</b> · ` : "";
  const agents = node?.agents || [];
  const agentAttr = agents.length ? ` data-agents="${agents.join(",")}"` : "";
  // chips double as toggles: dimmed = overlay hidden (persisted)
  const agentLegend = agents.length
    ? `<div class="${LEGEND}"><span>agents (click to toggle):</span>${agents
        .map((a, i) => `<button data-agent="${a}" class="flex items-center cursor-pointer hover:text-accent${
          hiddenAgents.has(a) ? " opacity-40 line-through" : ""
        }"><span class="${CHIP}" style="background:${agentColor(i)}"></span>${a}</button>`)
        .join("")}</div>`
    : "";
  view.innerHTML = `
    <div class="${HEAD}">
      <h1 class="${H1}">${title}</h1>
      <div class="${SUB}">${host}${path} · ${META.pings} pings every ${META.step}s</div>
    </div>
    ${legendHtml()}${agentLegend}
    ${RANGES.map(([r, label]) => `
      <div class="${CARD}">
        <h3 class="${CARD_H3}">${label}</h3>
        <div class="relative"><canvas class="graph" data-path="${path}" data-range="${r}"${agentAttr}></canvas></div>
      </div>`).join("")}`;
  for (const btn of view.querySelectorAll("button[data-agent]")) {
    btn.onclick = () => {
      const a = btn.dataset.agent;
      hiddenAgents.has(a) ? hiddenAgents.delete(a) : hiddenAgents.add(a);
      localStorage.setItem("dabping:agents-off", JSON.stringify([...hiddenAgents]));
      viewDetail(path); // re-render: chips restyle, graphs reload sans/avec overlay
    };
  }
  for (const c of view.querySelectorAll("canvas")) {
    charts.add(c);
    loadGraph(c);
  }
}

/// SmokePing charts mode: top targets by median / loss.
async function viewCharts() {
  markActive(null);
  const view = document.getElementById("view");
  view.innerHTML = `<div class="${HEAD}"><h1 class="${H1}">charts</h1>
    <div class="${SUB}">ranked by the latest round</div></div><div id="charts-lists"></div>`;
  const lists = document.getElementById("charts-lists");
  for (const [by, title] of [["median", "Slowest (median RTT)"], ["loss", "Lossiest"]]) {
    const entries = await fetch(`/api/charts/top?by=${by}&n=8`).then((r) => r.json()).catch(() => []);
    const sec = document.createElement("div");
    sec.innerHTML = `<div class="${HEAD} mt-4"><h1 class="m-0 text-[15px] font-bold tracking-wider">${title}</h1></div>
      <div class="${GRID}">${entries
        .map(
          (e) => `
        <div class="${GRID_CARD}">
          <a class="font-bold" href="#/t/${e.path}">${e.path}</a>
          <div class="text-[11px] text-base-content/55 mt-0.5 mb-2">${e.host} · ${fmtMs(e.median)} · loss ${e.loss.toFixed(1)}%</div>
          <div class="relative"><canvas class="graph mini" data-path="${e.path}" data-range="3h"></canvas></div>
        </div>`
        )
        .join("")}</div>`;
    lists.appendChild(sec);
  }
  for (const c of view.querySelectorAll("canvas")) {
    charts.add(c);
    loadGraph(c);
  }
}

/// Multi-host compare: every leaf's median on one graph per range.
async function viewCompare(path) {
  markActive(null);
  const node = findNode(path);
  const view = document.getElementById("view");
  if (!node) { view.innerHTML = `<div class="${HEAD}"><h1 class="${H1}">not found</h1></div>`; return; }
  const leaves = leavesUnder(node).slice(0, AGENT_COLORS.length);
  const legend = leaves
    .map((l, i) => `<span><span class="${CHIP}" style="background:${agentColor(i)}"></span>${nodeLabel(l)}</span>`)
    .join("");
  view.innerHTML = `
    <div class="${HEAD}">
      <h1 class="${H1}">${node.title || nodeLabel(node)} — compare</h1>
      <div class="${SUB}"><a href="#/s/${path}">back to overview</a></div>
    </div>
    <div class="${LEGEND}"><span>median lines:</span>${legend}</div>
    ${RANGES.slice(0, 2).map(([r, label]) => `
      <div class="${CARD}"><h3 class="${CARD_H3}">${label}</h3>
      <div class="relative"><canvas class="graph" id="cmp-${r}"></canvas></div></div>`).join("")}`;
  for (const [r] of RANGES.slice(0, 2)) {
    const canvas = document.getElementById(`cmp-${r}`);
    const points = Math.min(400, Math.max(50, Math.floor(canvas.clientWidth / 2)));
    const series = await Promise.all(
      leaves.map((l) =>
        fetch(`/api/data/${l.path}?range=${r}&points=${points}`)
          .then((resp) => (resp.ok ? resp.json() : null))
          .catch(() => null)
      )
    );
    drawCompare(canvas, leaves, series);
  }
}

function drawCompare(canvas, leaves, series) {
  // scale axes over every series, then reuse the agent-series renderer
  let top = 0;
  let x0 = Infinity, x1 = 0, period = 60;
  for (const f of series) {
    if (!f?.points?.length) continue;
    period = f.period;
    x0 = Math.min(x0, f.points[0].ts);
    x1 = Math.max(x1, f.points[f.points.length - 1].ts + f.period);
    for (const p of f.points) if (Number.isFinite(p.median)) top = Math.max(top, p.median * 1000);
  }
  if (!Number.isFinite(x0) || x1 <= x0) {
    drawEmpty(canvas.parentElement, "no data yet");
    return;
  }
  // axes from the combined bounds, then one overlay line per target;
  // the series ride along so log mode can derive a sane bottom decade
  drawSmoke(
    canvas,
    { period: x1 - x0, points: [{ ts: x0, loss: NaN, median: NaN, pings: [top / 1000] }] },
    { axesOnly: true, agents: series.filter(Boolean).map((f) => ({ fetched: f })) }
  );
  for (const [i, f] of series.entries()) {
    if (f) drawAgentSeries(canvas, f, agentColor(i)); // medians only, dashed
  }
}

function viewSection(path) {
  markActive(null);
  const node = findNode(path);
  const view = document.getElementById("view");
  if (!node) { view.innerHTML = `<div class="${HEAD}"><h1 class="${H1}">not found</h1></div>`; return; }
  const leaves = leavesUnder(node);
  view.innerHTML = `
    <div class="${HEAD}">
      <h1 class="${H1}">${node.title || nodeLabel(node)}</h1>
      <div class="${SUB}">${leaves.length} targets · last 3 hours · <a href="#/cmp/${path}">compare</a></div>
    </div>
    ${legendHtml()}
    <div class="${GRID}">
      ${leaves.map((l) => `
        <div class="${GRID_CARD}">
          <a class="font-bold" href="#/t/${l.path}">${nodeLabel(l)}</a>
          <div class="text-[11px] text-base-content/55 mt-0.5 mb-2">${l.host}</div>
          <div class="relative"><canvas class="graph mini" data-path="${l.path}" data-range="3h"></canvas></div>
        </div>`).join("")}
    </div>`;
  for (const c of view.querySelectorAll("canvas")) {
    charts.add(c);
    loadGraph(c);
  }
}

/* ---------- tooltip ---------- */

function initTooltip() {
  const tip = document.getElementById("tooltip");
  document.addEventListener("mousemove", (e) => {
    const c = e.target;
    if (!(c instanceof HTMLCanvasElement) || !c._chart) { tip.style.display = "none"; return; }
    const { pts, period, x0, span, M, W } = c._chart;
    const rect = c.getBoundingClientRect();
    const px = e.clientX - rect.left;
    if (px < M.l || px > W - M.r) { tip.style.display = "none"; return; }
    const t = x0 + ((px - M.l) / (W - M.l - M.r)) * span;
    const i = Math.min(pts.length - 1, Math.max(0, Math.floor((t - x0) / period)));
    const p = pts[i];
    const v = p.pings.filter(isFinite);
    tip.innerHTML = p && Number.isFinite(p.loss)
      ? `<b>${fmtFull(p.ts)}</b><br>median ${fmtMs(p.median)} · loss ${p.loss.toFixed(1)}%<br>` +
        (v.length ? `smoke ${fmtMs(v[0] )} – ${fmtMs(v[v.length - 1])}` : "no replies")
      : `<b>${fmtFull(p.ts)}</b><br>no data`;
    tip.style.display = "block";
    tip.style.left = Math.min(e.clientX + 14, window.innerWidth - tip.offsetWidth - 8) + "px";
    tip.style.top = (e.clientY + 14) + "px";
  });
}

/* ---------- live feed ---------- */

function connectLive() {
  const dot = document.getElementById("live-dot");
  const text = document.getElementById("live-text");
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/api/live`);
  ws.onopen = () => { dot.classList.add("on"); text.textContent = "live"; };
  ws.onmessage = (ev) => {
    try {
      const r = JSON.parse(ev.data);
      if (r.type !== "round") return;
      text.textContent = `${r.target} ${fmtMs(r.median)} / ${r.loss.toFixed(0)}%`;
      for (const c of charts) {
        if (c.isConnected && c.dataset.path === r.target && c.dataset.range === "3h")
          scheduleRefresh(c);
      }
    } catch { /* ignore malformed frames */ }
  };
  ws.onclose = () => {
    dot.classList.remove("on");
    text.textContent = "reconnecting…";
    setTimeout(connectLive, 3000);
  };
}

/* ---------- boot ---------- */

window.addEventListener("hashchange", route);
window.addEventListener("resize", (() => {
  let t;
  return () => { clearTimeout(t); t = setTimeout(redrawAll, 150); };
})());

(async function boot() {
  initThemePicker();
  initYScale();
  initTooltip();
  initZoom();
  try {
    const [meta, tree] = await Promise.all([
      fetch("/api/meta").then((r) => r.json()),
      fetch("/api/tree").then((r) => r.json()),
    ]);
    META = meta;
    TREE = tree;
  } catch {
    document.getElementById("view").innerHTML =
      `<div class="${HEAD}"><h1 class="${H1}">api unreachable</h1></div>`;
    return;
  }
  renderTree();
  route();
  connectLive();
  if (location.search.includes("themedd")) // screenshot hook, like ?snap
    document.getElementById("theme-dd").open = true;
  // long ranges don't ride the live feed; refresh everything visible periodically
  setInterval(() => { for (const c of charts) if (c.isConnected) loadGraph(c); },
    Math.max(60, META.step) * 1000);
})();
