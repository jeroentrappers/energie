// Small SVG charts for the dashboard: time series (line, area, step, column) on one or two
// y-axes, a "nu" marker, horizontal annotations, threshold colours, extremes and a hover
// readout; a gauge. No dependencies.
"use strict";
const Charts = (() => {
  const NS = "http://www.w3.org/2000/svg";
  const fmt = (v, d) => v === null || v === undefined || Number.isNaN(v) ? "–"
    : (Math.round(Number(v) * 10 ** d) === 0 ? 0 : Number(v)).toLocaleString(I18N.locale, { minimumFractionDigits: d, maximumFractionDigits: d });
  const el = (name, attrs, parent) => {
    const e = document.createElementNS(NS, name);
    for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v);
    if (parent) parent.appendChild(e);
    return e;
  };
  const text = (parent, x, y, s, attrs) => { const t = el("text", Object.assign({ x, y }, attrs), parent); t.textContent = s; return t; };

  function niceStep(span, n) {
    const raw = span / Math.max(1, n), p = Math.pow(10, Math.floor(Math.log10(raw))), f = raw / p;
    return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * p;
  }

  function extent(cfg, axis) {
    const a = cfg.axes[axis] || {};
    let lo = Infinity, hi = -Infinity;
    for (const s of cfg.series) {
      if ((s.axis || 0) !== axis) continue;
      for (const [, v] of s.data) if (v !== null && v !== undefined) { lo = Math.min(lo, v); hi = Math.max(hi, v); }
      if (s.type === "column" || s.type === "area") { lo = Math.min(lo, 0); hi = Math.max(hi, 0); }
    }
    for (const an of cfg.annotations || []) if ((an.axis || 0) === axis) { lo = Math.min(lo, an.y); hi = Math.max(hi, an.y); }
    if (!Number.isFinite(lo)) { lo = 0; hi = 1; }
    if (a.min !== undefined) lo = a.min;
    if (a.max !== undefined) hi = a.max;
    if (hi === lo) hi = lo + 1;
    const step = niceStep(hi - lo, 4);
    if (a.min === undefined) lo = Math.floor(lo / step) * step;
    if (a.max === undefined) hi = Math.ceil(hi / step) * step;
    return { lo, hi, step };
  }

  const TICKS = [5, 10, 15, 30, 60, 120, 180, 360, 720, 1440, 2880, 10080].map((m) => m * 60000);
  function timeTicks(from, to, width) {
    const want = Math.max(2, Math.floor(width / 70));
    const iv = TICKS.find((i) => (to - from) / i <= want) || TICKS[TICKS.length - 1];
    const out = [];
    const d = new Date(from);
    if (iv >= 86400000) d.setHours(0, 0, 0, 0); else if (iv >= 3600000) d.setMinutes(0, 0, 0); else d.setSeconds(0, 0);
    let t = d.getTime();
    while (t < from) t += iv;
    for (; t <= to; t += iv) {
      const x = new Date(t);
      if (iv >= 3600000 && iv < 86400000 && x.getHours() % (iv / 3600000) !== 0) continue;
      out.push(t);
    }
    return { ticks: out, days: iv >= 86400000 };
  }
  const tlabel = (t, days) => days
    ? new Date(t).toLocaleDateString(I18N.locale, { day: "numeric", month: "numeric" })
    : new Date(t).toLocaleTimeString(I18N.locale, { hour: "2-digit", minute: "2-digit" });

  function colorFor(s, v) {
    if (!s.thresholds) return s.color;
    let c = s.thresholds[0].color;
    for (const th of s.thresholds) if (v >= th.value) c = th.color;
    return c;
  }

  function lastValue(s) {
    for (let i = s.data.length - 1; i >= 0; i--) if (s.data[i][1] !== null && s.data[i][1] !== undefined) return s.data[i][1];
    return null;
  }

  // cfg: { from, to, now, height, bucket (ms, column width), axes: [{min, max, dec, title}],
  //   series: [{name, color, type: line|area|step|column, axis, data: [[ms, v]], opacity, width,
  //   thresholds: [{value, color}], extremas, dec, unit, header: false}], annotations: [{y, axis,
  //   color, label}], states: true }
  function chart(host, source) {
    const cfg = Object.assign({}, source, {
      series: source.series.map((s) => Object.assign({}, s, { name: I18N.T(s.name) })),
      axes: source.axes.map((a) => Object.assign({}, a, { title: I18N.T(a.title) })),
      annotations: (source.annotations || []).map((a) => Object.assign({}, a, { label: I18N.T(a.label) })),
    });
    host.innerHTML = "";
    host.classList.add("chart");
    if (cfg.states !== false) {
      const head = document.createElement("div");
      head.className = "states";
      for (const s of cfg.series) {
        if (s.header === false) continue;
        const v = lastValue(s);
        const span = document.createElement("span");
        span.innerHTML = `<i style="background:${s.color}"></i>${s.name} <b style="color:${s.color}">${fmt(v, s.dec ?? 1)}</b> <small>${s.unit || ""}</small>`;
        head.appendChild(span);
      }
      host.appendChild(head);
    }
    const W = Math.max(280, host.clientWidth || 600), H = cfg.height || 220;
    const two = cfg.axes.length > 1;
    const m = { l: 46, r: two ? 50 : 12, t: cfg.axes.some((x) => x.title) ? 20 : 10, b: 22 };
    const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", height: H, role: "img" });
    host.appendChild(svg);
    const from = cfg.from, to = cfg.to;
    const X = (t) => m.l + (t - from) / (to - from) * (W - m.l - m.r);
    const ex = [extent(cfg, 0), two ? extent(cfg, 1) : null];
    const Y = (v, axis) => { const e = ex[axis || 0]; return m.t + (1 - (v - e.lo) / (e.hi - e.lo)) * (H - m.t - m.b); };

    // Grid, axes.
    const g = el("g", { class: "grid" }, svg);
    const e0 = ex[0];
    for (let v = e0.lo; v <= e0.hi + e0.step / 1e6; v += e0.step) {
      el("line", { x1: m.l, x2: W - m.r, y1: Y(v, 0), y2: Y(v, 0) }, g);
      text(svg, m.l - 4, Y(v, 0) + 4, fmt(v, cfg.axes[0].dec ?? 0), { class: "ylab", "text-anchor": "end" });
    }
    if (two) {
      const e1 = ex[1];
      for (let v = e1.lo; v <= e1.hi + e1.step / 1e6; v += e1.step) {
        text(svg, W - m.r + 4, Y(v, 1) + 4, fmt(v, cfg.axes[1].dec ?? 0), { class: "ylab" });
      }
    }
    cfg.axes.forEach((a, i) => {
      if (a.title) text(svg, i ? W - 4 : 4, 2, a.title, { class: "atitle", "text-anchor": i ? "end" : "start", "dominant-baseline": "hanging" });
    });
    const tt = timeTicks(from, to, W - m.l - m.r);
    for (const t of tt.ticks) {
      el("line", { x1: X(t), x2: X(t), y1: m.t, y2: H - m.b, class: "vgrid" }, g);
      text(svg, X(t), H - 6, tlabel(t, tt.days), { class: "xlab", "text-anchor": "middle" });
    }

    const clip = "c" + Math.random().toString(36).slice(2);
    el("rect", { x: m.l, y: 0, width: W - m.l - m.r, height: H - m.b }, el("clipPath", { id: clip }, el("defs", {}, svg)));
    const plot = el("g", { "clip-path": `url(#${clip})` }, svg);

    // Columns first, then areas, then lines on top.
    const order = { column: 0, area: 1, step: 2, line: 3 };
    const sorted = cfg.series.map((s, i) => [s, i]).sort((a, b) => order[a[0].type || "line"] - order[b[0].type || "line"]);
    const cols = cfg.series.filter((s) => s.type === "column");
    for (const [s] of sorted) {
      const axis = s.axis || 0, pts = s.data.filter(([t]) => t >= from - (cfg.bucket || 0) && t <= to);
      if (s.type === "column") {
        const k = cols.indexOf(s), n = cols.length;
        const bw = (X(from + (s.bucket || cfg.bucket || 900000)) - X(from)) * 0.8 / n;
        const y0 = Y(Math.max(ex[axis].lo, Math.min(0, ex[axis].hi)), axis);
        for (const [t, v] of pts) {
          if (v === null) continue;
          const x = X(t) + (X(t + (s.bucket || cfg.bucket || 900000)) - X(t)) * 0.1 + k * bw, y = Y(v, axis);
          el("rect", { x, y: Math.min(y, y0), width: Math.max(1, bw - 1), height: Math.max(0.5, Math.abs(y0 - y)), fill: colorFor(s, v), opacity: s.opacity ?? 0.9 }, plot);
        }
        if (s.extremas) {
          const vals = pts.filter(([, v]) => v !== null);
          if (vals.length) {
            const lo = vals.reduce((a, b) => (b[1] < a[1] ? b : a)), hi = vals.reduce((a, b) => (b[1] > a[1] ? b : a));
            for (const [t, v] of [lo, hi]) text(plot, X(t) + bw / 2, Y(v, axis) - 4, fmt(v, s.dec ?? 3), { class: "extrema", "text-anchor": "middle" });
          }
        }
        continue;
      }
      let d = "";
      const segs = [];
      let seg = [];
      pts.forEach(([t, v], i) => {
        if (v === null || v === undefined) { if (seg.length) segs.push(seg); seg = []; return; }
        if (s.type === "step") {
          const nt = i + 1 < pts.length ? pts[i + 1][0] : t + (s.bucket || cfg.bucket || 900000);
          seg.push([X(t), Y(v, axis)], [X(Math.min(nt, to)), Y(v, axis)]);
        } else seg.push([X(t), Y(v, axis)]);
      });
      if (seg.length) segs.push(seg);
      for (const sg of segs) {
        d = sg.map(([x, y], j) => (j ? "L" : "M") + x.toFixed(1) + "," + y.toFixed(1)).join("");
        if (s.type === "area") {
          const yb = Y(Math.max(ex[axis].lo, Math.min(0, ex[axis].hi)), axis);
          el("path", { d: d + `L${sg[sg.length - 1][0].toFixed(1)},${yb}L${sg[0][0].toFixed(1)},${yb}Z`, fill: s.color, opacity: s.opacity ?? 0.3, stroke: "none" }, plot);
        }
        el("path", { d, fill: "none", stroke: s.color, "stroke-width": s.width ?? 2, "stroke-linejoin": "round", opacity: s.type === "area" ? 1 : (s.opacity ?? 1) }, plot);
      }
    }

    for (const an of cfg.annotations || []) {
      const y = Y(an.y, an.axis || 0);
      el("line", { x1: m.l, x2: W - m.r, y1: y, y2: y, stroke: an.color, "stroke-dasharray": "4 4", "stroke-width": 1.5 }, svg);
      if (an.label) {
        const lw = an.label.length * 6.2 + 8;
        el("rect", { x: W - m.r - lw, y: y - 16, width: lw, height: 14, rx: 3, fill: an.color }, svg);
        text(svg, W - m.r - lw / 2, y - 5, an.label, { class: "alab", "text-anchor": "middle" });
      }
    }
    if (cfg.now && cfg.now >= from && cfg.now <= to) {
      el("line", { x1: X(cfg.now), x2: X(cfg.now), y1: m.t, y2: H - m.b, class: "now" }, svg);
      text(svg, X(cfg.now) + 3, m.t + 10, I18N.T("nu"), { class: "nowlab" });
    }

    // Hover readout.
    const tip = document.createElement("div");
    tip.className = "tip hidden";
    host.appendChild(tip);
    const cross = el("line", { y1: m.t, y2: H - m.b, class: "cross", visibility: "hidden" }, svg);
    const move = (ev) => {
      const r = svg.getBoundingClientRect();
      const x = (ev.clientX - r.left) * W / r.width;
      if (x < m.l || x > W - m.r) { tip.classList.add("hidden"); cross.setAttribute("visibility", "hidden"); return; }
      const t = from + (x - m.l) / (W - m.l - m.r) * (to - from);
      const rows = [];
      let at = null;
      for (const s of cfg.series) {
        let best = null;
        for (const p of s.data) {
          const span = s.type === "column" || s.type === "step" ? (s.bucket || cfg.bucket || 900000) : 0;
          if (p[0] <= t && (span ? t < p[0] + span : true)) best = p;
          if (p[0] > t) { if (!span && (!best || p[0] - t < t - best[0])) best = p; break; }
        }
        if (best && best[1] !== null) { rows.push(`<span style="color:${s.color}">●</span> ${s.name}: <b>${fmt(best[1], s.dec ?? 1)}</b> ${s.unit || ""}`); at = at ?? best[0]; }
      }
      cross.setAttribute("x1", x); cross.setAttribute("x2", x); cross.setAttribute("visibility", "visible");
      if (!rows.length) { tip.classList.add("hidden"); return; }
      const when = new Date(at ?? t);
      tip.innerHTML = `<div>${when.toLocaleString(I18N.locale, { weekday: "short", hour: "2-digit", minute: "2-digit" })}</div>` + rows.join("<br>");
      tip.classList.remove("hidden");
      const px = (ev.clientX - r.left);
      tip.style.left = Math.min(px + 12, r.width - tip.offsetWidth - 4) + "px";
      tip.style.top = (svg.offsetTop + 8) + "px";
    };
    svg.addEventListener("pointermove", move);
    svg.addEventListener("pointerdown", move);
    svg.addEventListener("pointerleave", () => { tip.classList.add("hidden"); cross.setAttribute("visibility", "hidden"); });

    // Dragging across the chart selects a time range (`cfg.onselect(fromMs, toMs)`).
    if (cfg.onselect) {
      svg.style.touchAction = "pan-y";
      svg.style.cursor = "crosshair";
      const band = el("rect", { y: m.t, height: H - m.t - m.b, fill: "var(--accent)", opacity: 0.2, visibility: "hidden" }, svg);
      const px = (ev) => { const r = svg.getBoundingClientRect(); return Math.min(W - m.r, Math.max(m.l, (ev.clientX - r.left) * W / r.width)); };
      const at = (x) => from + (x - m.l) / (W - m.l - m.r) * (to - from);
      let start = null;
      svg.addEventListener("pointerdown", (ev) => {
        start = px(ev);
        svg.setPointerCapture(ev.pointerId);
        band.setAttribute("x", start); band.setAttribute("width", 0); band.setAttribute("visibility", "visible");
      });
      svg.addEventListener("pointermove", (ev) => {
        if (start === null) return;
        const x = px(ev);
        band.setAttribute("x", Math.min(start, x)); band.setAttribute("width", Math.abs(x - start));
      });
      svg.addEventListener("pointerup", (ev) => {
        if (start === null) return;
        const a = Math.min(start, px(ev)), b = Math.max(start, px(ev));
        start = null;
        tip.classList.add("hidden");
        if (b - a < 6) { band.setAttribute("visibility", "hidden"); return; }
        cfg.onselect(at(a), at(b));
      });
    }
  }

  // { value, min, max, unit, dec, label, severity: [{from, color}] }
  function gauge(host, g) {
    host.innerHTML = "";
    const svg = el("svg", { viewBox: "0 0 200 125", width: "100%", class: "gauge", role: "img" }, host);
    const cx = 100, cy = 100, r = 80;
    const ang = (v) => Math.PI * (1 - (Math.min(g.max, Math.max(g.min, v)) - g.min) / (g.max - g.min));
    const pt = (a, rr) => [cx + rr * Math.cos(a), cy - rr * Math.sin(a)];
    const arc = (v0, v1, color) => {
      const [x0, y0] = pt(ang(v0), r), [x1, y1] = pt(ang(v1), r);
      el("path", { d: `M${x0},${y0} A${r},${r} 0 0 1 ${x1},${y1}`, stroke: color, "stroke-width": 16, fill: "none" }, svg);
    };
    const sev = g.severity || [{ from: g.min, color: "var(--accent)" }];
    sev.forEach((s, i) => arc(Math.max(g.min, s.from), i + 1 < sev.length ? sev[i + 1].from : g.max, s.color));
    if (g.value !== null && g.value !== undefined) {
      const [nx, ny] = pt(ang(g.value), r - 4);
      el("line", { x1: cx, y1: cy, x2: nx, y2: ny, class: "needle" }, svg);
      el("circle", { cx, cy, r: 5, class: "needlehub" }, svg);
    }
    text(svg, cx, cy - 22, fmt(g.value, g.dec ?? 0) + (g.unit ? " " + g.unit : ""), { class: "gval", "text-anchor": "middle" });
    text(svg, cx - r, cy + 16, fmt(g.min, 0), { class: "glab", "text-anchor": "middle" });
    text(svg, cx + r, cy + 16, fmt(g.max, 0), { class: "glab", "text-anchor": "middle" });
    if (g.label) text(svg, cx, cy + 20, I18N.T(g.label), { class: "glabel", "text-anchor": "middle" });
  }

  return { chart, gauge, fmt };
})();
