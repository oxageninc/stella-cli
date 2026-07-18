//! `/export` — export all session telemetry as a ZIP archive of raw JSON
//! dumps plus a self-contained HTML dashboard.
//!
//! The archive lives at `.stella/exports/` and is named with the microsecond
//! timestamp of the **last log entry** included (the data's own clock, not the
//! user's submission time). The HTML is fully static — no external CSS/JS,
//! everything inlined — so it can be opened offline, emailed, or committed
//! alongside a PR as evidence.
//!
//! The dashboard surfaces the metrics that actually change software quality:
//! resolve rate, cost-per-resolved-task, token efficiency, tool-call
//! frequency, retry patterns, and file-edit heat — the same data `stella
//! stats` summarizes in a table, but visually and interactively.

use std::path::{Path, PathBuf};

use stella_store::Store;

/// One `(table_name, json_array)` pair from the export dump.
type TableDump = (&'static str, String);

/// Build the export archive. Returns the path to the written file, or an
/// error message. `workspace_root` is where `.stella/exports/` is created.
pub fn export_session(workspace_root: &Path) -> Result<PathBuf, String> {
    let store = Store::open(workspace_root).map_err(|e| format!("cannot open store: {e}"))?;

    // Collect every table's raw data.
    let dumps = store
        .export_all_json()
        .map_err(|e| format!("cannot read telemetry: {e}"))?;

    if dumps.iter().all(|(_, json)| json == "[]") {
        return Err("no session telemetry recorded yet — run a few turns first.".into());
    }

    // The watermark: the timestamp of the last log entry in this set. Falls
    // back to "now" only if the store somehow has no timestamps at all.
    let watermark = store
        .last_log_timestamp()
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            // SQLite's CURRENT_TIMESTAMP is second-resolution; we need
            // microsecond precision for a unique, sortable filename. Use
            // SystemTime as the final fallback.
            use std::time::{SystemTime, UNIX_EPOCH};
            let micros = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros())
                .unwrap_or(0);
            format!("{micros}")
        });

    // Sanitize the watermark into a filename-safe folder name.
    let folder = sanitize_timestamp(&watermark);

    let usage_stats = store
        .usage_stats()
        .map_err(|e| format!("cannot read usage stats: {e}"))?;

    // Build the self-contained HTML dashboard.
    let html = render_dashboard(&usage_stats, &dumps, &watermark);

    // Assemble the ZIP.
    let exports_dir = workspace_root.join(".stella").join("exports");
    std::fs::create_dir_all(&exports_dir).map_err(|e| format!("create exports dir: {e}"))?;
    let zip_path = exports_dir.join(format!("session-{folder}.zip"));

    let mut zip = ZipWriter::new();
    // Raw JSON dumps — one per table, inside the timestamped folder.
    for (table, json) in &dumps {
        let pretty = pretty_json(json);
        zip.add_file(&format!("{folder}/raw/{table}.json"), pretty.as_bytes());
    }
    // The dashboard.
    zip.add_file(&format!("{folder}/dashboard.html"), html.as_bytes());
    // A manifest with the watermark and table list.
    let manifest = serde_json::json!({
        "exported_at": watermark,
        "tables": dumps.iter().map(|(t, j)| {
            let count = serde_json::from_str::<Vec<serde_json::Value>>(j)
                .map(|v| v.len())
                .unwrap_or(0);
            serde_json::json!({"table": t, "rows": count})
        }).collect::<Vec<_>>(),
    });
    zip.add_file(
        &format!("{folder}/manifest.json"),
        serde_json::to_string_pretty(&manifest)
            .unwrap_or_default()
            .as_bytes(),
    );

    let bytes = zip.finish();
    std::fs::write(&zip_path, &bytes).map_err(|e| format!("write archive: {e}"))?;

    Ok(zip_path)
}

/// Format an integer with comma thousands separators (e.g. `1234567` →
/// `1,234,567`). Rust's format strings don't support `:,`, so we do it here.
fn comma(n: i64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let neg = n < 0;
    let digits = if neg { &bytes[1..] } else { bytes };
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in digits.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    if neg { format!("-{out}") } else { out }
}

/// Sanitize a timestamp string for use as a directory name: strip anything
/// that isn't alphanumeric, dash, or underscore, and collapse runs.
fn sanitize_timestamp(ts: &str) -> String {
    let clean: String = ts
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of dashes (e.g. "2024-01-15 10:30:00" → "2024-01-15-10-30-00").
    let mut result = String::with_capacity(clean.len());
    let mut prev_dash = false;
    for c in clean.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }
    result.trim_matches('-').to_string()
}

/// Pretty-print a compact JSON string (best-effort — falls back to raw).
fn pretty_json(compact: &str) -> String {
    serde_json::from_str::<serde_json::Value>(compact)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| compact.to_string())
}

// ── Self-contained HTML dashboard ───────────────────────────────────────────

/// Render the full HTML dashboard. All CSS and JS are inlined — no external
/// dependencies. The data is embedded as a JSON blob so the JS can build
/// interactive charts client-side.
fn render_dashboard(
    usage_stats: &[stella_store::UsageStatsRow],
    dumps: &[TableDump],
    watermark: &str,
) -> String {
    let total_cost: f64 = usage_stats.iter().map(|r| r.total_cost_usd).sum();
    let total_runs: i64 = usage_stats.iter().map(|r| r.runs).sum();
    let total_resolved: i64 = usage_stats.iter().map(|r| r.resolved).sum();
    let total_input: i64 = usage_stats.iter().map(|r| r.input_tokens).sum();
    let total_output: i64 = usage_stats.iter().map(|r| r.output_tokens).sum();
    let total_cache_read: i64 = usage_stats.iter().map(|r| r.cache_read_tokens).sum();
    let resolve_rate = if total_runs > 0 {
        total_resolved as f64 / total_runs as f64 * 100.0
    } else {
        0.0
    };
    let cost_per_resolved = if total_resolved > 0 {
        total_cost / total_resolved as f64
    } else {
        0.0
    };

    // Pre-format integers with comma separators (Rust's format! doesn't
    // support `:,` like Python's).
    let total_input_fmt = comma(total_input);
    let total_output_fmt = comma(total_output);
    let total_cache_read_fmt = comma(total_cache_read);

    // Telemetry rows for the timeline chart.
    let telemetry_json = dumps
        .iter()
        .find(|(t, _)| *t == "telemetry")
        .map(|(_, j)| j.as_str())
        .unwrap_or("[]");

    // Tool-call frequency.
    let tool_calls_json = dumps
        .iter()
        .find(|(t, _)| *t == "tool_calls")
        .map(|(_, j)| j.as_str())
        .unwrap_or("[]");

    // Executions (for the outcome breakdown).
    let executions_json = dumps
        .iter()
        .find(|(t, _)| *t == "executions")
        .map(|(_, j)| j.as_str())
        .unwrap_or("[]");

    // Files touched.
    let files_json = dumps
        .iter()
        .find(|(t, _)| *t == "files_touched")
        .map(|(_, j)| j.as_str())
        .unwrap_or("[]");

    // Usage stats as JSON for the per-model table.
    let stats_json = serde_json::to_string(usage_stats).unwrap_or_else(|_| "[]".into());

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Stella Session Telemetry — {watermark}</title>
<style>
  :root {{
    --bg: #0b0d16;
    --surface: #15131f;
    --raised: #1e1a2e;
    --text: #f5f4f2;
    --text2: #b6afc9;
    --text3: #7e7791;
    --gold: #f9d423;
    --flame: #ff7e5f;
    --crimson: #c2185b;
    --violet: #a78bfa;
    --success: #3fd69b;
    --warn: #f4b24a;
    --rule: #241b33;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    background: var(--bg); color: var(--text);
    line-height: 1.5; padding: 24px; max-width: 1280px; margin: 0 auto;
  }}
  h1 {{ font-size: 1.8rem; margin-bottom: 4px; color: var(--text); }}
  h2 {{ font-size: 1.25rem; margin: 32px 0 12px; color: var(--gold); border-bottom: 1px solid var(--rule); padding-bottom: 8px; }}
  .watermark {{ color: var(--text3); font-size: 0.85rem; margin-bottom: 24px; font-family: monospace; }}
  .kpi-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 12px; margin-bottom: 8px; }}
  .kpi {{
    background: var(--surface); border: 1px solid var(--rule); border-radius: 8px; padding: 16px;
  }}
  .kpi .label {{ font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text3); margin-bottom: 4px; }}
  .kpi .value {{ font-size: 1.8rem; font-weight: 700; }}
  .kpi .sub {{ font-size: 0.75rem; color: var(--text2); margin-top: 2px; }}
  .kpi.good .value {{ color: var(--success); }}
  .kpi.warn .value {{ color: var(--warn); }}
  .kpi.cost .value {{ color: var(--flame); }}
  table {{ width: 100%; border-collapse: collapse; background: var(--surface); border-radius: 8px; overflow: hidden; }}
  th, td {{ padding: 8px 12px; text-align: left; font-size: 0.85rem; border-bottom: 1px solid var(--rule); }}
  th {{ background: var(--raised); color: var(--text2); font-weight: 600; font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.05em; }}
  tr:last-child td {{ border-bottom: none; }}
  td.num {{ text-align: right; font-variant-numeric: tabular-nums; font-family: monospace; }}
  .badge {{ display: inline-block; padding: 1px 6px; border-radius: 3px; font-size: 0.7rem; font-weight: 600; }}
  .badge.completed {{ background: rgba(63,214,155,0.15); color: var(--success); }}
  .badge.failed {{ background: rgba(229,83,123,0.15); color: var(--crimson); }}
  .badge.other {{ background: rgba(184,175,201,0.15); color: var(--text2); }}
  .chart-container {{ background: var(--surface); border: 1px solid var(--rule); border-radius: 8px; padding: 16px; margin-bottom: 16px; overflow-x: auto; }}
  .bar-chart {{ display: flex; flex-direction: column; gap: 4px; }}
  .bar-row {{ display: flex; align-items: center; gap: 8px; font-size: 0.8rem; }}
  .bar-row .bar-label {{ width: 200px; text-align: right; color: var(--text2); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
  .bar-row .bar-track {{ flex: 1; background: var(--raised); border-radius: 3px; height: 22px; position: relative; min-width: 100px; }}
  .bar-row .bar-fill {{ height: 100%; border-radius: 3px; background: var(--violet); transition: width 0.3s; }}
  .bar-row .bar-value {{ width: 60px; color: var(--text3); font-family: monospace; font-size: 0.75rem; }}
  .pie-legend {{ display: flex; gap: 16px; flex-wrap: wrap; margin-top: 8px; font-size: 0.8rem; }}
  .pie-legend span {{ display: flex; align-items: center; gap: 4px; }}
  .dot {{ width: 10px; height: 10px; border-radius: 2px; display: inline-block; }}
  .insight {{ background: var(--surface); border-left: 3px solid var(--gold); padding: 12px 16px; border-radius: 0 8px 8px 0; margin-bottom: 8px; font-size: 0.9rem; }}
  .insight .insight-label {{ color: var(--gold); font-weight: 600; font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; }}
  .footer {{ margin-top: 40px; padding-top: 16px; border-top: 1px solid var(--rule); color: var(--text3); font-size: 0.75rem; }}
</style>
</head>
<body>

<h1>⚡ Stella Session Telemetry</h1>
<div class="watermark">as of {watermark}</div>

<div class="kpi-grid">
  <div class="kpi"><div class="label">Total Runs</div><div class="value">{total_runs}</div><div class="sub">{total_resolved} resolved</div></div>
  <div class="kpi good"><div class="label">Resolve Rate</div><div class="value">{resolve_rate:.1}%</div><div class="sub">{total_resolved}/{total_runs}</div></div>
  <div class="kpi cost"><div class="label">Total Cost</div><div class="value">${total_cost:.4}</div><div class="sub">${cost_per_resolved:.4}/resolved</div></div>
  <div class="kpi"><div class="label">Tokens In</div><div class="value">{total_input_fmt}</div><div class="sub">{total_cache_read_fmt} cache reads</div></div>
  <div class="kpi"><div class="label">Tokens Out</div><div class="value">{total_output_fmt}</div><div class="sub">generated</div></div>
</div>

<div id="insights"></div>

<h2>Cost &amp; Efficiency by Model</h2>
<div id="stats-table"></div>

<h2>Token Economy</h2>
<div class="chart-container">
  <div id="token-chart" class="bar-chart"></div>
</div>

<h2>Tool Usage</h2>
<div class="chart-container">
  <div id="tool-chart" class="bar-chart"></div>
</div>

<h2>Files Touched</h2>
<div class="chart-container">
  <div id="file-chart" class="bar-chart"></div>
</div>

<h2>Execution Outcomes</h2>
<div class="chart-container">
  <div id="outcome-chart" class="bar-chart"></div>
</div>

<div class="footer">
  Exported by <strong>stella /export</strong> · {total_runs} executions ·
  All data is local (no server, no account) · Dashboard is fully self-contained
</div>

<script>
const USAGE = {stats_json};
const TELEMETRY = {telemetry_json};
const TOOL_CALLS = {tool_calls_json};
const EXECUTIONS = {executions_json};
const FILES = {files_json};

// ── KPI insights — surface the patterns that change quality ─────────────
(function insights() {{
  const el = document.getElementById('insights');
  const tips = [];

  // Cache hit rate.
  const totalIn = USAGE.reduce((s,r)=>s+r.input_tokens,0);
  const cacheRead = USAGE.reduce((s,r)=>s+r.cache_read_tokens,0);
  if (totalIn > 0) {{
    const rate = (cacheRead/totalIn*100).toFixed(1);
    if (rate > 50) tips.push({{label:'Cache Efficiency',text:`Prompt caching is saving ${{rate}}% of input tokens — the session is reusing context well.`}});
    else if (totalIn > 10000) tips.push({{label:'Cache Opportunity',text:`Only ${{rate}}% of input tokens were cache reads. Longer, stable system prompts with cache breakpoints would cut cost.`}});
  }}

  // Resolve rate.
  const runs = USAGE.reduce((s,r)=>s+r.runs,0);
  const resolved = USAGE.reduce((s,r)=>s+r.resolved,0);
  const rate = runs > 0 ? resolved/runs*100 : 0;
  if (runs >= 3) {{
    if (rate >= 80) tips.push({{label:'High Resolve Rate',text:`${{rate.toFixed(0)}}% of turns resolved successfully — the prompts and model are well-matched.`}});
    else if (rate < 50) tips.push({{label:'Low Resolve Rate',text:`Only ${{rate.toFixed(0)}}% of turns resolved. Consider clearer prompts, a stronger model, or the staged pipeline (/pipeline).`}});
  }}

  // Cost efficiency.
  const cost = USAGE.reduce((s,r)=>s+r.total_cost_usd,0);
  if (resolved > 0 && cost > 0) {{
    const per = (cost/resolved).toFixed(4);
    tips.push({{label:'Cost per Resolution',text:`Average $${{per}} per resolved task across all models.`}});
  }}

  // Most expensive model vs cheapest.
  if (USAGE.length > 1) {{
    const sorted = [...USAGE].sort((a,b)=>b.total_cost_usd-a.total_cost_usd);
    const top = sorted[0];
    if (top.total_cost_usd > 0) tips.push({{label:'Cost Concentration',text:`${{top.provider}}/${{top.model}} accounts for $${{top.total_cost_usd.toFixed(4)}} (${{(top.total_cost_usd/cost*100).toFixed(0)}}% of total spend).`}});
  }}

  // Retries — signal from telemetry.
  const retries = TELEMETRY.reduce((s,t)=>s+(t.retries||0),0);
  if (retries > 5) tips.push({{label:'Retry Pressure',text:`${{retries}} API retries this session — may indicate rate limiting or transient errors.`}});

  el.innerHTML = tips.map(t=>`<div class="insight"><div class="insight-label">${{t.label}}</div>${{t.text}}</div>`).join('');
}})();

// ── Stats table ─────────────────────────────────────────────────────────
(function statsTable() {{
  const el = document.getElementById('stats-table');
  if (!USAGE.length) {{ el.innerHTML = '<p style="color:var(--text3)">No usage data.</p>'; return; }}
  let html = '<table><thead><tr><th>Provider</th><th>Model</th><th class="num">Runs</th><th class="num">Resolved</th><th class="num">Rate</th><th class="num">Cost</th><th class="num">$/Resolved</th><th class="num">In Tok</th><th class="num">Out Tok</th><th class="num">Avg ms</th></tr></thead><tbody>';
  for (const r of USAGE) {{
    const rate = r.runs > 0 ? (r.resolved/r.runs*100).toFixed(1)+'%' : '-';
    const perResolved = r.cost_per_resolved_usd != null ? '$'+r.cost_per_resolved_usd.toFixed(4) : '-';
    html += `<tr><td>${{r.provider}}</td><td>${{r.model}}</td><td class="num">${{r.runs}}</td><td class="num">${{r.resolved}}</td><td class="num">${{rate}}</td><td class="num">$${{r.total_cost_usd.toFixed(4)}}</td><td class="num">${{perResolved}}</td><td class="num">${{r.input_tokens.toLocaleString()}}</td><td class="num">${{r.output_tokens.toLocaleString()}}</td><td class="num">${{Math.round(r.avg_duration_ms)}}</td></tr>`;
  }}
  // Totals.
  const runs = USAGE.reduce((s,r)=>s+r.runs,0);
  const resolved = USAGE.reduce((s,r)=>s+r.resolved,0);
  const cost = USAGE.reduce((s,r)=>s+r.total_cost_usd,0);
  const inTok = USAGE.reduce((s,r)=>s+r.input_tokens,0);
  const outTok = USAGE.reduce((s,r)=>s+r.output_tokens,0);
  const rate = runs>0?(resolved/runs*100).toFixed(1)+'%':'-';
  const per = resolved>0?'$'+(cost/resolved).toFixed(4):'-';
  html += `<tr style="border-top:2px solid var(--rule)"><td colspan="2"><strong>TOTAL</strong></td><td class="num"><strong>${{runs}}</strong></td><td class="num"><strong>${{resolved}}</strong></td><td class="num"><strong>${{rate}}</strong></td><td class="num"><strong>$${{cost.toFixed(4)}}</strong></td><td class="num"><strong>${{per}}</strong></td><td class="num"><strong>${{inTok.toLocaleString()}}</strong></td><td class="num"><strong>${{outTok.toLocaleString()}}</strong></td><td class="num">—</td></tr>`;
  html += '</tbody></table>';
  el.innerHTML = html;
}})();

// ── Bar chart helper ────────────────────────────────────────────────────
function barChart(containerId, data, colorVar) {{
  const el = document.getElementById(containerId);
  if (!data.length) {{ el.innerHTML = '<p style="color:var(--text3)">No data.</p>'; return; }}
  const max = Math.max(...data.map(d=>d.value), 1);
  el.innerHTML = data.map(d => {{
    const pct = (d.value/max*100).toFixed(1);
    return `<div class="bar-row"><div class="bar-label" title="${{d.label}}">${{d.label}}</div><div class="bar-track"><div class="bar-fill" style="width:${{pct}}%;background:var(${{colorVar}})"></div></div><div class="bar-value">${{d.display}}</div></div>`;
  }}).join('');
}}

// ── Token economy chart ─────────────────────────────────────────────────
barChart('token-chart', USAGE.map(r=>({{label:r.provider+'/'+r.model, value:r.input_tokens, display:r.input_tokens.toLocaleString()}})), '--violet');

// ── Tool frequency chart ────────────────────────────────────────────────
(function toolChart() {{
  const counts = {{}};
  for (const c of TOOL_CALLS) {{ counts[c.name] = (counts[c.name]||0)+1; }}
  const data = Object.entries(counts)
    .map(([name,n])=>({{label:name, value:n, display:String(n)}}))
    .sort((a,b)=>b.value-a.value)
    .slice(0,15);
  barChart('tool-chart', data, '--gold');
}})();

// ── Files touched chart ─────────────────────────────────────────────────
(function fileChart() {{
  const data = FILES
    .map(f=>({{label:f.path, value:(f.lines_added||0)+(f.lines_removed||0), display:'+'+(f.lines_added||0)+'/-'+(f.lines_removed||0)}}))
    .sort((a,b)=>b.value-a.value)
    .slice(0,15);
  barChart('file-chart', data, '--flame');
}})();

// ── Execution outcomes ──────────────────────────────────────────────────
(function outcomeChart() {{
  const counts = {{}};
  for (const e of EXECUTIONS) {{
    const o = e.outcome || 'open';
    counts[o] = (counts[o]||0)+1;
  }}
  const data = Object.entries(counts)
    .map(([name,n])=>({{label:name, value:n, display:String(n)}}))
    .sort((a,b)=>b.value-a.value);
  barChart('outcome-chart', data, '--success');
}})();
</script>

</body>
</html>"##
    )
}

// ── Minimal ZIP writer (store-only, no compression) ─────────────────────────
//
// We avoid a `zip` crate dependency by writing the simplest valid ZIP: stored
// (uncompressed) entries with correct CRC-32, local file headers, central
// directory, and end-of-central-directory record. This is fully compatible
// with every unzip tool and OS file explorer. Store-only is fine here — the
// raw JSON compresses poorly anyway relative to the simplicity cost.

/// CRC-32 lookup table (polynomial 0xEDB88320).
fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        table[i as usize] = c;
    }
    table
}

/// Compute CRC-32 for a byte slice.
fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFFFFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFFFFFF
}

/// A minimal stored-entry ZIP writer.
struct ZipWriter {
    entries: Vec<ZipEntry>,
    data: Vec<u8>,
}

struct ZipEntry {
    name: String,
    offset: u32,
    crc32: u32,
    size: u32,
}

impl ZipWriter {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            data: Vec::new(),
        }
    }

    fn add_file(&mut self, name: &str, content: &[u8]) {
        let crc = crc32(content);
        let offset = self.data.len() as u32;
        let size = content.len() as u32;

        // Local file header (PK\x03\x04)
        self.data.extend_from_slice(&[
            0x50, 0x4b, 0x03, 0x04, // signature
            0x14, 0x00, // version needed (2.0)
            0x00, 0x00, // flags
            0x00, 0x00, // compression: stored
            0x00, 0x00, // mod time
            0x00, 0x00, // mod date
        ]);
        self.data.extend_from_slice(&crc.to_le_bytes());
        self.data.extend_from_slice(&size.to_le_bytes()); // compressed size
        self.data.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        let name_bytes = name.as_bytes();
        self.data
            .extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        self.data.extend_from_slice(name_bytes);
        self.data.extend_from_slice(content);

        self.entries.push(ZipEntry {
            name: name.to_string(),
            offset,
            crc32: crc,
            size,
        });
    }

    fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.data.len() as u32;

        // Central directory file headers (PK\x01\x02)
        for entry in &self.entries {
            self.data.extend_from_slice(&[
                0x50, 0x4b, 0x01, 0x02, // signature
                0x14, 0x00, // version made by
                0x14, 0x00, // version needed
                0x00, 0x00, // flags
                0x00, 0x00, // compression: stored
                0x00, 0x00, // mod time
                0x00, 0x00, // mod date
            ]);
            self.data.extend_from_slice(&entry.crc32.to_le_bytes());
            self.data.extend_from_slice(&entry.size.to_le_bytes());
            self.data.extend_from_slice(&entry.size.to_le_bytes());
            let name_bytes = entry.name.as_bytes();
            self.data
                .extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            self.data.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.data.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.data.extend_from_slice(&0u16.to_le_bytes()); // disk number
            self.data.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.data.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.data.extend_from_slice(&entry.offset.to_le_bytes());
            self.data.extend_from_slice(name_bytes);
        }

        let cd_size = (self.data.len() as u32) - cd_offset;

        // End of central directory (PK\x05\x06)
        self.data.extend_from_slice(&[
            0x50, 0x4b, 0x05, 0x06, // signature
            0x00, 0x00, // disk number
            0x00, 0x00, // disk with CD
        ]);
        let count = self.entries.len() as u16;
        self.data.extend_from_slice(&count.to_le_bytes()); // entries on this disk
        self.data.extend_from_slice(&count.to_le_bytes()); // total entries
        self.data.extend_from_slice(&cd_size.to_le_bytes());
        self.data.extend_from_slice(&cd_offset.to_le_bytes());
        self.data.extend_from_slice(&0u16.to_le_bytes()); // comment length

        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_writer_produces_a_valid_archive() {
        let mut zip = ZipWriter::new();
        zip.add_file("a.txt", b"hello");
        zip.add_file("b.txt", b"world!!!");
        let bytes = zip.finish();

        // Minimum valid ZIP with 2 entries: 2 local headers + 2 central + EOCD.
        // Each local header is 30 + name_len; central is 46 + name_len; EOCD is 22.
        assert!(bytes.len() > 100);
        // Starts with PK\x03\x04.
        assert_eq!(&bytes[..4], &[0x50, 0x4b, 0x03, 0x04]);
        // Ends with EOCD PK\x05\x06.
        assert_eq!(
            &bytes[bytes.len() - 22..bytes.len() - 18],
            &[0x50, 0x4b, 0x05, 0x06]
        );

        // Verify the content is stored verbatim (store-only).
        let hello_pos = find_subsequence(&bytes, b"hello");
        assert!(hello_pos.is_some(), "file content stored in the zip");
    }

    #[test]
    fn zip_writer_handles_empty_files() {
        let mut zip = ZipWriter::new();
        zip.add_file("empty.txt", b"");
        let bytes = zip.finish();
        assert_eq!(&bytes[..4], &[0x50, 0x4b, 0x03, 0x04]);
    }

    #[test]
    fn crc32_matches_known_values() {
        // CRC-32 of "hello" is 0x3610a686.
        assert_eq!(crc32(b"hello"), 0x3610a686);
        // CRC-32 of "" is 0.
        assert_eq!(crc32(b""), 0);
        // CRC-32 of "123456789" is 0xcbf43926 (the standard check value).
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
    }

    #[test]
    fn sanitize_timestamp_strips_unsafe_chars() {
        assert_eq!(
            sanitize_timestamp("2024-01-15 10:30:00"),
            "2024-01-15-10-30-00"
        );
        assert_eq!(sanitize_timestamp("1705312200000000"), "1705312200000000");
        assert_eq!(sanitize_timestamp("../etc/passwd"), "etc-passwd");
    }

    #[test]
    fn sanitize_timestamp_collapses_dash_runs() {
        assert_eq!(sanitize_timestamp("a--b---c"), "a-b-c");
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn export_all_json_from_an_in_memory_store() {
        let store = Store::in_memory().unwrap();
        // An empty store should produce empty arrays, not errors.
        let dumps = store.export_all_json().unwrap();
        assert!(
            dumps.iter().all(|(_, j)| j == "[]"),
            "an empty store exports empty arrays"
        );
    }

    #[test]
    fn export_round_trips_through_a_real_store() {
        // Record a minimal execution + telemetry, then export and verify the
        // JSON round-trips.
        let store = Store::in_memory().unwrap();
        use stella_store::{FileTouchRow, TelemetryRow};

        // We need an execution id — use the internal record path via a direct
        // SQL insert since start_execution is on the CLI side.
        store
            .record_telemetry(
                1,
                &TelemetryRow {
                    step: 0,
                    provider: "test".into(),
                    model: "test-model".into(),
                    input_tokens: 100,
                    estimated_input_tokens: 90,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_miss_tokens: 100,
                    cache_write_tokens: 10,
                    cost_usd: 0.001,
                    duration_ms: 500,
                    retries: 0,
                    tool_calls: 1,
                },
            )
            .unwrap();
        store
            .record_files_touched(
                1,
                &[FileTouchRow {
                    path: "src/main.rs".into(),
                    ops: "M".into(),
                    lines_added: 10,
                    lines_removed: 2,
                    events_json: "[]".into(),
                }],
            )
            .unwrap();

        let dumps = store.export_all_json().unwrap();
        let telemetry = dumps
            .iter()
            .find(|(t, _)| *t == "telemetry")
            .map(|(_, j)| j.as_str())
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(telemetry).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["provider"], "test");
        assert_eq!(parsed[0]["input_tokens"], 100);

        let files = dumps
            .iter()
            .find(|(t, _)| *t == "files_touched")
            .map(|(_, j)| j.as_str())
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(files).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "src/main.rs");
    }

    #[test]
    fn full_export_pipeline_creates_valid_zip_with_dashboard() {
        // End-to-end: seed a temp store with data, export it, and verify the
        // archive is a valid ZIP containing the HTML dashboard and raw JSON.
        use stella_store::TelemetryRow;
        let tmp = tempfile::tempdir().unwrap();

        // Seed: record telemetry so the store has data to export.
        {
            let store = Store::open(tmp.path()).unwrap();
            store
                .record_telemetry(
                    1,
                    &TelemetryRow {
                        step: 0,
                        provider: "anthropic".into(),
                        model: "claude-test".into(),
                        input_tokens: 1000,
                        estimated_input_tokens: 900,
                        output_tokens: 200,
                        cache_read_tokens: 500,
                        cache_miss_tokens: 500,
                        cache_write_tokens: 10,
                        cost_usd: 0.012,
                        duration_ms: 1500,
                        retries: 1,
                        tool_calls: 3,
                    },
                )
                .unwrap();
        }

        // Export.
        let zip_path = export_session(tmp.path()).expect("export should succeed");
        assert!(zip_path.exists(), "the zip file was written");
        assert!(
            zip_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("session-"),
            "filename starts with session-"
        );
        assert!(
            zip_path.extension().is_some_and(|e| e == "zip"),
            "extension is .zip"
        );

        // Verify the archive content.
        let bytes = std::fs::read(&zip_path).unwrap();
        assert_eq!(&bytes[..2], b"PK", "valid ZIP magic bytes");
        let content = String::from_utf8_lossy(&bytes);
        assert!(
            content.contains("Stella Session Telemetry"),
            "HTML dashboard is embedded"
        );
        assert!(
            content.contains("anthropic"),
            "provider name appears in the data"
        );
        assert!(
            content.contains("claude-test"),
            "model name appears in the data"
        );
        assert!(content.contains("manifest"), "manifest is present");
    }

    #[test]
    fn full_export_pipeline_errors_on_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the store (so .stella/ exists) but record nothing.
        let _ = Store::open(tmp.path()).unwrap();
        let result = export_session(tmp.path());
        assert!(result.is_err(), "exporting an empty store is an error");
        assert!(
            result.unwrap_err().contains("no session telemetry"),
            "error message is helpful"
        );
    }
}
