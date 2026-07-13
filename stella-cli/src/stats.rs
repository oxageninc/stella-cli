//! `stella stats` — cost/$-per-resolved-task analytics straight from the
//! workspace's local DuckDB telemetry (`.stella/stella.duckdb`).
//!
//! Reads what `stella-store` recorded — nothing else. Works with zero API
//! keys configured (no provider is ever resolved), never writes: if the
//! database file doesn't exist yet it says so instead of creating one.
//!
//! Formats: an aligned table for humans (with a TOTAL row), and json/csv
//! for machine-readable receipts (Arena submissions want these). Field
//! order in json/csv follows `UsageStatsRow`'s declaration order — a
//! stable contract.

use clap::ValueEnum;
use stella_store::{Store, UsageStatsRow};

/// Output format for `stella stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StatsFormat {
    /// Aligned human-readable columns with a TOTAL row (default).
    Table,
    /// Pretty-printed JSON array of per-(provider, model) rows.
    Json,
    /// RFC-4180-style CSV with a header row.
    Csv,
}

/// Entry point for `stella stats`. `provider` filters rows to one provider
/// id (e.g. `zai`, `anthropic`, `local`); `None` shows everything.
pub fn run_stats(format: StatsFormat, provider: Option<&str>) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;

    // Stats is read-only: opening the store would create `.stella/` as a
    // side effect, so bail out politely when there's nothing to read.
    let db_path = workspace_root.join(".stella").join("stella.duckdb");
    let rows = if db_path.exists() {
        let store =
            Store::open(&workspace_root).map_err(|e| format!("cannot open local store: {e}"))?;
        let mut rows = store
            .usage_stats()
            .map_err(|e| format!("cannot read usage stats: {e}"))?;
        if let Some(p) = provider {
            rows.retain(|r| r.provider == p);
        }
        rows
    } else {
        Vec::new()
    };

    if rows.is_empty() {
        // An empty store is a state, not an error — but keep stdout valid
        // for the machine formats.
        match format {
            StatsFormat::Table => println!("{}", empty_message(provider)),
            StatsFormat::Json => {
                eprintln!("{}", empty_message(provider));
                println!("[]");
            }
            StatsFormat::Csv => {
                eprintln!("{}", empty_message(provider));
                println!("{}", csv_header());
            }
        }
        return Ok(());
    }

    match format {
        StatsFormat::Table => print!("{}", render_table(&rows)),
        StatsFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| format!("serialize: {e}"))?
        ),
        StatsFormat::Csv => print!("{}", render_csv(&rows)),
    }
    Ok(())
}

fn empty_message(provider: Option<&str>) -> String {
    match provider {
        Some(p) => format!(
            "No executions recorded for provider `{p}` yet — run `stella run \"...\"` (or \
             chat/goal) to generate local telemetry."
        ),
        None => "No executions recorded yet — run `stella run \"...\"` (or chat/goal) to \
                 generate local telemetry in .stella/stella.duckdb."
            .to_string(),
    }
}

/// The TOTAL row across every displayed row, computed in Rust (weighted,
/// not an average of averages): rates and $/resolved are re-derived from
/// the summed counts, and $/resolved stays `-` when nothing resolved.
fn totals(rows: &[UsageStatsRow]) -> UsageStatsRow {
    let runs: i64 = rows.iter().map(|r| r.runs).sum();
    let resolved: i64 = rows.iter().map(|r| r.resolved).sum();
    let total_cost_usd: f64 = rows.iter().map(|r| r.total_cost_usd).sum();
    let total_duration_ms: f64 = rows.iter().map(|r| r.avg_duration_ms * r.runs as f64).sum();
    UsageStatsRow {
        provider: "TOTAL".into(),
        model: "-".into(),
        division: "-".into(),
        runs,
        resolved,
        resolve_rate: if runs > 0 {
            resolved as f64 / runs as f64
        } else {
            0.0
        },
        total_cost_usd,
        cost_per_resolved_usd: (resolved > 0).then(|| total_cost_usd / resolved as f64),
        input_tokens: rows.iter().map(|r| r.input_tokens).sum(),
        output_tokens: rows.iter().map(|r| r.output_tokens).sum(),
        cache_read_tokens: rows.iter().map(|r| r.cache_read_tokens).sum(),
        cache_write_tokens: rows.iter().map(|r| r.cache_write_tokens).sum(),
        avg_duration_ms: if runs > 0 {
            total_duration_ms / runs as f64
        } else {
            0.0
        },
    }
}

/// Column headers, in `UsageStatsRow` field order.
const TABLE_HEADERS: [&str; 13] = [
    "PROVIDER",
    "MODEL",
    "DIVISION",
    "RUNS",
    "RESOLVED",
    "RATE",
    "COST ($)",
    "$/RESOLVED",
    "IN TOK",
    "OUT TOK",
    "CACHE RD",
    "CACHE WR",
    "AVG MS",
];

/// The first three columns are strings (left-aligned); the rest are
/// numbers (right-aligned).
const STRING_COLS: usize = 3;

fn table_cells(row: &UsageStatsRow) -> [String; 13] {
    [
        row.provider.clone(),
        row.model.clone(),
        row.division.clone(),
        row.runs.to_string(),
        row.resolved.to_string(),
        format!("{:.1}%", row.resolve_rate * 100.0),
        format!("{:.4}", row.total_cost_usd),
        match row.cost_per_resolved_usd {
            Some(v) => format!("{v:.4}"),
            None => "-".into(),
        },
        row.input_tokens.to_string(),
        row.output_tokens.to_string(),
        row.cache_read_tokens.to_string(),
        row.cache_write_tokens.to_string(),
        format!("{:.0}", row.avg_duration_ms),
    ]
}

/// Render the aligned table with a separator line before the TOTAL row.
fn render_table(rows: &[UsageStatsRow]) -> String {
    let body: Vec<[String; 13]> = rows.iter().map(table_cells).collect();
    let total = table_cells(&totals(rows));

    let mut widths: Vec<usize> = TABLE_HEADERS.iter().map(|h| h.len()).collect();
    for cells in body.iter().chain(std::iter::once(&total)) {
        for (w, cell) in widths.iter_mut().zip(cells.iter()) {
            *w = (*w).max(cell.len());
        }
    }

    let render_line = |cells: &[String]| -> String {
        let cols: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                if i < STRING_COLS {
                    format!("{cell:<width$}", width = widths[i])
                } else {
                    format!("{cell:>width$}", width = widths[i])
                }
            })
            .collect();
        // Two-space gutter; trim so left-aligned last cells never leave
        // trailing whitespace.
        cols.join("  ").trim_end().to_string()
    };

    let mut out = String::new();
    let headers: Vec<String> = TABLE_HEADERS.iter().map(|h| h.to_string()).collect();
    out.push_str(&render_line(&headers));
    out.push('\n');
    for cells in &body {
        out.push_str(&render_line(cells));
        out.push('\n');
    }
    let rule_width = widths.iter().sum::<usize>() + 2 * (widths.len() - 1);
    out.push_str(&"-".repeat(rule_width));
    out.push('\n');
    out.push_str(&render_line(&total));
    out.push('\n');
    out
}

/// CSV header, matching `UsageStatsRow`'s serialized field order exactly.
fn csv_header() -> &'static str {
    "provider,model,division,runs,resolved,resolve_rate,total_cost_usd,cost_per_resolved_usd,\
     input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,avg_duration_ms"
}

/// Quote a CSV field when it contains a comma, quote, or line break;
/// embedded quotes are doubled (RFC 4180).
fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn csv_row(row: &UsageStatsRow) -> String {
    format!(
        "{},{},{},{},{},{:.4},{:.6},{},{},{},{},{},{:.1}",
        csv_escape(&row.provider),
        csv_escape(&row.model),
        csv_escape(&row.division),
        row.runs,
        row.resolved,
        row.resolve_rate,
        row.total_cost_usd,
        match row.cost_per_resolved_usd {
            Some(v) => format!("{v:.6}"),
            None => String::new(),
        },
        row.input_tokens,
        row.output_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
        row.avg_duration_ms,
    )
}

fn render_csv(rows: &[UsageStatsRow]) -> String {
    let mut out = String::from(csv_header());
    out.push('\n');
    for row in rows {
        out.push_str(&csv_row(row));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, model: &str) -> UsageStatsRow {
        UsageStatsRow {
            provider: provider.into(),
            model: model.into(),
            division: UsageStatsRow::division_for_provider(provider).into(),
            runs: 4,
            resolved: 2,
            resolve_rate: 0.5,
            total_cost_usd: 0.03,
            cost_per_resolved_usd: Some(0.015),
            input_tokens: 6000,
            output_tokens: 600,
            cache_read_tokens: 2000,
            cache_write_tokens: 10,
            avg_duration_ms: 750.0,
        }
    }

    #[test]
    fn csv_escapes_quotes_and_commas() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn csv_row_matches_header_order_and_blanks_unresolved() {
        let mut r = row("zai", "glm-5.2");
        assert_eq!(
            csv_header().split(',').count(),
            csv_row(&r).split(',').count()
        );
        assert_eq!(
            csv_row(&r),
            "zai,glm-5.2,-,4,2,0.5000,0.030000,0.015000,6000,600,2000,10,750.0"
        );

        // resolved = 0 → the $/resolved field is EMPTY, never 0 or NaN.
        r.resolved = 0;
        r.resolve_rate = 0.0;
        r.cost_per_resolved_usd = None;
        assert_eq!(
            csv_row(&r),
            "zai,glm-5.2,-,4,0,0.0000,0.030000,,6000,600,2000,10,750.0"
        );

        // Fields with commas round-trip quoted.
        r.model = "weird,model".into();
        assert!(csv_row(&r).starts_with("zai,\"weird,model\",-,"));
    }

    #[test]
    fn totals_row_is_weighted_not_average_of_averages() {
        let mut a = row("zai", "glm-5.2");
        let mut b = row("anthropic", "claude-fable-5");
        b.runs = 1;
        b.resolved = 0;
        b.resolve_rate = 0.0;
        b.total_cost_usd = 0.05;
        b.cost_per_resolved_usd = None;
        b.avg_duration_ms = 100.0;
        a.avg_duration_ms = 750.0;

        let t = totals(&[a, b]);
        assert_eq!(t.provider, "TOTAL");
        assert_eq!(t.runs, 5);
        assert_eq!(t.resolved, 2);
        assert!((t.resolve_rate - 0.4).abs() < 1e-12);
        assert!((t.total_cost_usd - 0.08).abs() < 1e-12);
        assert!((t.cost_per_resolved_usd.unwrap() - 0.04).abs() < 1e-12);
        // (750*4 + 100*1) / 5 = 620 — weighted by runs.
        assert!((t.avg_duration_ms - 620.0).abs() < 1e-12);
    }

    #[test]
    fn totals_with_nothing_resolved_has_no_cost_per_resolved() {
        let mut a = row("anthropic", "claude-fable-5");
        a.resolved = 0;
        a.cost_per_resolved_usd = None;
        let t = totals(&[a]);
        assert_eq!(t.cost_per_resolved_usd, None);
    }

    #[test]
    fn table_aligns_columns_and_appends_total() {
        let out = render_table(&[row("zai", "glm-5.2"), row("local", "llama-3.3")]);
        let lines: Vec<&str> = out.lines().collect();
        // header + 2 rows + rule + TOTAL
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("PROVIDER"));
        assert!(lines[1].contains("50.0%"));
        assert!(lines[1].contains("0.0300"));
        assert!(lines[1].contains("0.0150"));
        assert!(lines[2].contains("off-grid"));
        assert!(lines[3].chars().all(|c| c == '-'));
        assert!(lines[4].starts_with("TOTAL"));
        // Every RATE cell ends at the same column — alignment witness.
        let col = lines[0].find("RATE").unwrap() + "RATE".len();
        assert!(lines[1][..col].ends_with("50.0%"));
        assert!(lines[2][..col].ends_with("50.0%"));
    }
}
