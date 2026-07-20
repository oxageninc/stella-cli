//! `catalog.db` — the **user-tier** model catalog: the master list of valid
//! provider/model slugs, their pricing, and every string form those slugs
//! travel under. One database per developer (next to `usage.db` in the OS
//! data dir), synced from the public models.dev master list by `stella
//! models refresh` and read by every command that must answer "is this
//! slug real, and what does it cost?".
//!
//! Three tables carry the model vocabulary:
//!
//! - **model_cards** — one row per model *as offered by an API provider*,
//!   keyed `(api_provider, slug)`. `api_provider` is who serves the
//!   request (stella's provider id: `anthropic`, `openrouter`, `bedrock`,
//!   …); `model_provider` is who makes the model (`anthropic` for a Claude
//!   card even when the card's api_provider is `bedrock`). Cards are keyed
//!   per API provider because pricing is set by whoever serves the model,
//!   not whoever made it.
//! - **model_card_versions** — the card's pricing configurations, append
//!   -only. A refresh inserts a NEW version only when the pricing
//!   configuration actually changed (content-hash compare), so the version
//!   history is real change history, not one row per sync. **The latest
//!   version is the pricing that displays everywhere.**
//! - **model_aliases** — the telemetry join table: any string form a model
//!   travels under (bare slug, `provider/slug`, date-stamped snapshot ids,
//!   region-prefixed Bedrock profiles, whatever a provider echoes on the
//!   wire) mapped to `(api_provider, model_provider, model_version,
//!   model_card_id)`. Telemetry keeps recording raw wire strings; joining
//!   them through this table is what makes per-model analytics exact.
//!
//! **catalog_sync** holds the incremental-refresh bookkeeping (the HTTP
//! `ETag` and payload hash per source), so an unchanged master list costs
//! one conditional request and zero writes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{Result, StoreError, fnv_hex};

/// Where the user-tier catalog lives: `data_dir()/catalog.db` (honors
/// `STELLA_DATA_DIR`, like every user-tier store).
pub fn catalog_db_path() -> PathBuf {
    crate::usage::data_dir().join("catalog.db")
}

const CATALOG_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS model_cards (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    api_provider   TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    slug           TEXT NOT NULL,
    display_name   TEXT,
    family         TEXT,
    source         TEXT NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(api_provider, slug)
);
CREATE TABLE IF NOT EXISTS model_card_versions (
    id                        INTEGER PRIMARY KEY AUTOINCREMENT,
    model_card_id             INTEGER NOT NULL REFERENCES model_cards(id) ON DELETE CASCADE,
    version                   INTEGER NOT NULL,
    input_usd_per_mtok        REAL,
    output_usd_per_mtok       REAL,
    cached_input_usd_per_mtok REAL,
    cache_write_usd_per_mtok  REAL,
    context_window            INTEGER,
    max_output_tokens         INTEGER,
    release_date              TEXT,
    last_updated              TEXT,
    source                    TEXT NOT NULL,
    content_hash              TEXT NOT NULL,
    captured_at               TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(model_card_id, version)
);
CREATE INDEX IF NOT EXISTS model_card_versions_by_card
    ON model_card_versions(model_card_id, version DESC);
CREATE TABLE IF NOT EXISTS model_aliases (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    alias          TEXT NOT NULL,
    api_provider   TEXT NOT NULL,
    model_provider TEXT NOT NULL,
    model_version  TEXT,
    model_card_id  INTEGER NOT NULL REFERENCES model_cards(id) ON DELETE CASCADE,
    source         TEXT NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(api_provider, alias)
);
CREATE INDEX IF NOT EXISTS model_aliases_by_card ON model_aliases(model_card_id);
CREATE TABLE IF NOT EXISTS catalog_sync (
    source       TEXT PRIMARY KEY,
    etag         TEXT,
    payload_hash TEXT,
    refreshed_at TEXT NOT NULL
);
";

/// Columns added after the first shipped schema. `CREATE TABLE IF NOT
/// EXISTS` never grows an existing table, so each (table, column, type)
/// here is applied with `ALTER TABLE … ADD COLUMN` when missing — the
/// SQLite-idiomatic in-place migration (nullable columns only, so old rows
/// stay valid and old binaries reading a new db just ignore the extras).
const CATALOG_MIGRATIONS: &[(&str, &str, &str)] = &[
    // Capability flags (0/1, NULL = unknown): whether the model can think
    // (reasoning/extended thinking) and whether it accepts tool definitions.
    // Synced from models.dev and from provider-native /models endpoints.
    ("model_card_versions", "supports_reasoning", "INTEGER"),
    ("model_card_versions", "supports_tools", "INTEGER"),
];

/// Apply [`CATALOG_MIGRATIONS`] to an open connection.
fn migrate(conn: &Connection) -> Result<()> {
    for (table, column, ty) in CATALOG_MIGRATIONS {
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |row| row.get(0),
        )?;
        if present == 0 {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
    }
    Ok(())
}

/// One pricing configuration — the payload of a model-card version. All
/// fields optional: the master list legitimately omits pricing for free or
/// gateway-priced models, and an unknown price must stay unknown (a `0.0`
/// would silently zero budget metering).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VersionData {
    pub input_usd_per_mtok: Option<f64>,
    pub output_usd_per_mtok: Option<f64>,
    pub cached_input_usd_per_mtok: Option<f64>,
    pub cache_write_usd_per_mtok: Option<f64>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    /// Whether the model supports reasoning / extended thinking. `None` is
    /// "unknown" and must stay `None` — an unknown capability degrades to
    /// today's permissive behavior, never to a hard "no".
    pub supports_reasoning: Option<bool>,
    /// Whether the model accepts tool definitions.
    pub supports_tools: Option<bool>,
}

impl VersionData {
    /// The change-detection hash: a new model-card version is created iff
    /// this differs from the latest stored version's hash. Covers the
    /// pricing configuration and token limits — NOT display metadata, so a
    /// renamed model doesn't fabricate a pricing-history entry.
    pub fn content_hash(&self) -> String {
        let mut key = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.input_usd_per_mtok,
            self.output_usd_per_mtok,
            self.cached_input_usd_per_mtok,
            self.cache_write_usd_per_mtok,
            self.context_window,
            self.max_output_tokens,
        );
        // Capabilities join the hash only when known, so every version
        // stored before the columns existed keeps its historical hash and a
        // re-sync of unchanged capability-less data appends nothing.
        if self.supports_reasoning.is_some() || self.supports_tools.is_some() {
            key.push_str(&format!(
                "|{:?}|{:?}",
                self.supports_reasoning, self.supports_tools
            ));
        }
        fnv_hex(&key)
    }
}

/// One string form a model travels under, attached to its card at upsert.
#[derive(Debug, Clone)]
pub struct AliasForm {
    pub alias: String,
    /// The version token embedded in this form, when it has one (the
    /// `20250929` of `claude-sonnet-4-5-20250929`, the `v1:0` of a Bedrock
    /// profile) — `None` for undated forms.
    pub model_version: Option<String>,
    /// Where the form came from: `catalog` (the master list's own id),
    /// `derived` (a generated variant), or `learned` (seen on the wire).
    pub source: String,
}

/// Everything one refresh knows about one model — card identity, the
/// current pricing configuration, and the string forms to register.
#[derive(Debug, Clone)]
pub struct ModelUpsert {
    pub api_provider: String,
    pub model_provider: String,
    pub slug: String,
    pub display_name: Option<String>,
    pub family: Option<String>,
    /// `models.dev` for master-list rows, `seed` for the in-binary floor.
    pub source: String,
    pub version: VersionData,
    pub aliases: Vec<AliasForm>,
}

/// What one batch changed — the refresh receipt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshCounts {
    pub models_seen: u64,
    pub cards_added: u64,
    pub versions_added: u64,
    pub aliases_added: u64,
}

/// A resolved slug/alias: the card it names plus the latest version's
/// pricing configuration (`version == 0` and an empty pricing block when
/// the card has no stored version yet).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model_card_id: i64,
    pub api_provider: String,
    pub model_provider: String,
    pub slug: String,
    pub display_name: Option<String>,
    pub family: Option<String>,
    /// The matched alias row's version token (e.g. `20250929`), not the
    /// card-version counter.
    pub model_version: Option<String>,
    pub matched_alias: String,
    /// The latest card-version counter — the pricing generation.
    pub version: i64,
    pub pricing: VersionData,
}

/// One row of `stella models list`: a card with its latest pricing.
#[derive(Debug, Clone)]
pub struct ModelListing {
    pub api_provider: String,
    pub model_provider: String,
    pub slug: String,
    pub display_name: Option<String>,
    pub family: Option<String>,
    pub source: String,
    pub version: i64,
    pub pricing: VersionData,
}

/// The incremental-refresh bookkeeping for one source.
#[derive(Debug, Clone)]
pub struct SyncInfo {
    pub etag: Option<String>,
    pub payload_hash: Option<String>,
    pub refreshed_at: String,
}

/// The user-tier model catalog. Same concurrency posture as every stella
/// store: WAL so readers never block on a writer, a `Mutex<Connection>`
/// serializing in-process writers, and a busy timeout for the rare
/// cross-process write race (two sessions learning an alias at once).
pub struct CatalogStore {
    conn: Mutex<Connection>,
}

impl CatalogStore {
    /// Open (creating if needed) the catalog at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        let _: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(CATALOG_SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open the default user-tier catalog at [`catalog_db_path`].
    pub fn open_default() -> Result<Self> {
        Self::open(&catalog_db_path())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Apply one refresh batch in a single transaction: upsert cards,
    /// append a new version per card whose pricing configuration changed,
    /// and register alias forms. Idempotent — re-applying an identical
    /// batch adds nothing.
    pub fn apply_batch(&self, models: &[ModelUpsert]) -> Result<RefreshCounts> {
        let mut counts = RefreshCounts::default();
        let mut guard = self.lock();
        let tx = guard.transaction()?;
        for model in models {
            counts.models_seen += 1;

            // Card: insert-or-update keyed (api_provider, slug). Metadata
            // (display name, family, model_provider, source) tracks the
            // latest sync; identity never changes.
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM model_cards WHERE api_provider = ?1 AND slug = ?2",
                    params![model.api_provider, model.slug],
                    |row| row.get(0),
                )
                .optional()?;
            let card_id = match existing {
                Some(id) => {
                    tx.execute(
                        "UPDATE model_cards SET
                             model_provider = ?2,
                             display_name   = COALESCE(?3, display_name),
                             family         = COALESCE(?4, family),
                             source         = ?5
                         WHERE id = ?1",
                        params![
                            id,
                            model.model_provider,
                            model.display_name,
                            model.family,
                            model.source
                        ],
                    )?;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO model_cards
                             (api_provider, model_provider, slug, display_name, family, source)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            model.api_provider,
                            model.model_provider,
                            model.slug,
                            model.display_name,
                            model.family,
                            model.source
                        ],
                    )?;
                    counts.cards_added += 1;
                    tx.last_insert_rowid()
                }
            };

            // Version: append-only, and ONLY on a real pricing change.
            let new_hash = model.version.content_hash();
            let latest: Option<(i64, String)> = tx
                .query_row(
                    "SELECT version, content_hash FROM model_card_versions
                     WHERE model_card_id = ?1 ORDER BY version DESC LIMIT 1",
                    params![card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let needs_version = match &latest {
                Some((_, hash)) => *hash != new_hash,
                None => true,
            };
            if needs_version {
                let next = latest.map(|(v, _)| v + 1).unwrap_or(1);
                tx.execute(
                    "INSERT INTO model_card_versions
                         (model_card_id, version, input_usd_per_mtok, output_usd_per_mtok,
                          cached_input_usd_per_mtok, cache_write_usd_per_mtok, context_window,
                          max_output_tokens, release_date, last_updated, source, content_hash,
                          supports_reasoning, supports_tools)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        card_id,
                        next,
                        model.version.input_usd_per_mtok,
                        model.version.output_usd_per_mtok,
                        model.version.cached_input_usd_per_mtok,
                        model.version.cache_write_usd_per_mtok,
                        model.version.context_window.map(|v| v as i64),
                        model.version.max_output_tokens.map(|v| v as i64),
                        model.version.release_date,
                        model.version.last_updated,
                        model.source,
                        new_hash,
                        model.version.supports_reasoning,
                        model.version.supports_tools
                    ],
                )?;
                counts.versions_added += 1;
            }

            // Aliases: caller-provided forms first (they may carry version
            // tokens), then the slug itself as a guaranteed fallback so a
            // card is always resolvable by its own id. OR IGNORE keeps the
            // first registration of a form — an exact catalog id can never
            // be overwritten by a later derived collision.
            for alias in &model.aliases {
                let changed = tx.execute(
                    "INSERT OR IGNORE INTO model_aliases
                         (alias, api_provider, model_provider, model_version, model_card_id, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        alias.alias,
                        model.api_provider,
                        model.model_provider,
                        alias.model_version,
                        card_id,
                        alias.source
                    ],
                )?;
                counts.aliases_added += changed as u64;
            }
            let changed = tx.execute(
                "INSERT OR IGNORE INTO model_aliases
                     (alias, api_provider, model_provider, model_version, model_card_id, source)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                params![
                    model.slug,
                    model.api_provider,
                    model.model_provider,
                    card_id,
                    model.source
                ],
            )?;
            counts.aliases_added += changed as u64;
        }
        tx.commit()?;
        Ok(counts)
    }

    /// Record a completed sync for `source` (e.g. `models.dev`): the ETag
    /// to replay as `If-None-Match`, the payload hash, and "now".
    pub fn record_sync(
        &self,
        source: &str,
        etag: Option<&str>,
        payload_hash: Option<&str>,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO catalog_sync (source, etag, payload_hash, refreshed_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(source) DO UPDATE SET
                 etag = excluded.etag,
                 payload_hash = excluded.payload_hash,
                 refreshed_at = excluded.refreshed_at",
            params![source, etag, payload_hash],
        )?;
        Ok(())
    }

    /// The last completed sync for `source`, if any. `None` means the
    /// source has never been synced on this machine — the signal the CLI
    /// uses both for validation strictness and for the auto-refresh
    /// opt-in (no sync row → never fetch implicitly).
    pub fn sync_info(&self, source: &str) -> Result<Option<SyncInfo>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT etag, payload_hash, refreshed_at FROM catalog_sync WHERE source = ?1",
                params![source],
                |row| {
                    Ok(SyncInfo {
                        etag: row.get(0)?,
                        payload_hash: row.get(1)?,
                        refreshed_at: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Seconds since the last completed sync for `source` — `None` when
    /// never synced (or the stored timestamp is unreadable).
    pub fn seconds_since_sync(&self, source: &str) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT CAST(strftime('%s','now') AS INTEGER)
                        - CAST(strftime('%s', refreshed_at) AS INTEGER)
                 FROM catalog_sync WHERE source = ?1",
                params![source],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Resolve any string form against the alias table, returning the card
    /// it names plus the latest version's pricing configuration.
    pub fn resolve(&self, api_provider: &str, alias: &str) -> Result<Option<ResolvedModel>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT c.id, c.api_provider, c.model_provider, c.slug, c.display_name, c.family,
                        a.model_version, a.alias,
                        COALESCE(v.version, 0),
                        v.input_usd_per_mtok, v.output_usd_per_mtok,
                        v.cached_input_usd_per_mtok, v.cache_write_usd_per_mtok,
                        v.context_window, v.max_output_tokens, v.release_date, v.last_updated,
                        v.supports_reasoning, v.supports_tools
                 FROM model_aliases a
                 JOIN model_cards c ON c.id = a.model_card_id
                 LEFT JOIN model_card_versions v ON v.model_card_id = c.id
                      AND v.version = (SELECT MAX(version) FROM model_card_versions
                                       WHERE model_card_id = c.id)
                 WHERE a.api_provider = ?1 AND a.alias = ?2",
                params![api_provider, alias],
                |row| {
                    Ok(ResolvedModel {
                        model_card_id: row.get(0)?,
                        api_provider: row.get(1)?,
                        model_provider: row.get(2)?,
                        slug: row.get(3)?,
                        display_name: row.get(4)?,
                        family: row.get(5)?,
                        model_version: row.get(6)?,
                        matched_alias: row.get(7)?,
                        version: row.get(8)?,
                        pricing: VersionData {
                            input_usd_per_mtok: row.get(9)?,
                            output_usd_per_mtok: row.get(10)?,
                            cached_input_usd_per_mtok: row.get(11)?,
                            cache_write_usd_per_mtok: row.get(12)?,
                            context_window: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                            max_output_tokens: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                            release_date: row.get(15)?,
                            last_updated: row.get(16)?,
                            supports_reasoning: row.get(17)?,
                            supports_tools: row.get(18)?,
                        },
                    })
                },
            )
            .optional()?)
    }

    /// How many cards an API provider has, optionally filtered by card
    /// source. The CLI's strictness gate asks `source = "models.dev"`:
    /// only master-list data (never the seed floor or learned rows) makes
    /// a provider's slug set authoritative.
    pub fn provider_model_count(&self, api_provider: &str, source: Option<&str>) -> Result<u64> {
        let conn = self.lock();
        let count: i64 = match source {
            Some(source) => conn.query_row(
                "SELECT COUNT(*) FROM model_cards WHERE api_provider = ?1 AND source = ?2",
                params![api_provider, source],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT COUNT(*) FROM model_cards WHERE api_provider = ?1",
                params![api_provider],
                |row| row.get(0),
            )?,
        };
        Ok(count as u64)
    }

    /// Every card for one API provider (or all providers), each with its
    /// latest pricing configuration — `stella models list` and the runtime
    /// catalog assembly both read this.
    pub fn models_for_provider(&self, api_provider: Option<&str>) -> Result<Vec<ModelListing>> {
        let conn = self.lock();
        let sql = "SELECT c.api_provider, c.model_provider, c.slug, c.display_name, c.family,
                          c.source, COALESCE(v.version, 0),
                          v.input_usd_per_mtok, v.output_usd_per_mtok,
                          v.cached_input_usd_per_mtok, v.cache_write_usd_per_mtok,
                          v.context_window, v.max_output_tokens, v.release_date, v.last_updated,
                          v.supports_reasoning, v.supports_tools
                   FROM model_cards c
                   LEFT JOIN model_card_versions v ON v.model_card_id = c.id
                        AND v.version = (SELECT MAX(version) FROM model_card_versions
                                         WHERE model_card_id = c.id)
                   WHERE (?1 IS NULL OR c.api_provider = ?1)
                   ORDER BY c.api_provider, c.slug";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![api_provider], |row| {
            Ok(ModelListing {
                api_provider: row.get(0)?,
                model_provider: row.get(1)?,
                slug: row.get(2)?,
                display_name: row.get(3)?,
                family: row.get(4)?,
                source: row.get(5)?,
                version: row.get(6)?,
                pricing: VersionData {
                    input_usd_per_mtok: row.get(7)?,
                    output_usd_per_mtok: row.get(8)?,
                    cached_input_usd_per_mtok: row.get(9)?,
                    cache_write_usd_per_mtok: row.get(10)?,
                    context_window: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                    max_output_tokens: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                    release_date: row.get(13)?,
                    last_updated: row.get(14)?,
                    supports_reasoning: row.get(15)?,
                    supports_tools: row.get(16)?,
                },
            })
        })?;
        let mut listings = Vec::new();
        for row in rows {
            listings.push(row?);
        }
        Ok(listings)
    }

    /// Register a wire-observed string form against an existing card — the
    /// telemetry-perfection hook. Returns `true` when the alias was new.
    /// The card's own `model_provider` is copied onto the alias row so the
    /// aliases table alone answers the full join (string → api provider +
    /// model provider + model version + model card).
    pub fn insert_learned_alias(
        &self,
        api_provider: &str,
        alias: &str,
        model_card_id: i64,
        model_version: Option<&str>,
    ) -> Result<bool> {
        let conn = self.lock();
        let model_provider: Option<String> = conn
            .query_row(
                "SELECT model_provider FROM model_cards WHERE id = ?1",
                params![model_card_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(model_provider) = model_provider else {
            return Err(StoreError(format!(
                "cannot learn alias `{alias}`: model card {model_card_id} does not exist"
            )));
        };
        let changed = conn.execute(
            "INSERT OR IGNORE INTO model_aliases
                 (alias, api_provider, model_provider, model_version, model_card_id, source)
             VALUES (?1, ?2, ?3, ?4, ?5, 'learned')",
            params![
                alias,
                api_provider,
                model_provider,
                model_version,
                model_card_id
            ],
        )?;
        Ok(changed > 0)
    }

    /// Table sizes: (cards, versions, aliases) — the receipt line for
    /// `stella models` and refresh reporting.
    pub fn counts(&self) -> Result<(u64, u64, u64)> {
        let conn = self.lock();
        let cards: i64 = conn.query_row("SELECT COUNT(*) FROM model_cards", [], |r| r.get(0))?;
        let versions: i64 =
            conn.query_row("SELECT COUNT(*) FROM model_card_versions", [], |r| r.get(0))?;
        let aliases: i64 =
            conn.query_row("SELECT COUNT(*) FROM model_aliases", [], |r| r.get(0))?;
        Ok((cards as u64, versions as u64, aliases as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CatalogStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(&dir.path().join("catalog.db")).expect("open");
        (dir, store)
    }

    fn sonnet_upsert() -> ModelUpsert {
        ModelUpsert {
            api_provider: "anthropic".to_string(),
            model_provider: "anthropic".to_string(),
            slug: "claude-sonnet-4-5".to_string(),
            display_name: Some("Claude Sonnet 4.5".to_string()),
            family: Some("claude-sonnet".to_string()),
            source: "models.dev".to_string(),
            version: VersionData {
                input_usd_per_mtok: Some(3.0),
                output_usd_per_mtok: Some(15.0),
                cached_input_usd_per_mtok: Some(0.3),
                cache_write_usd_per_mtok: Some(3.75),
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
                release_date: Some("2025-09-29".to_string()),
                last_updated: Some("2025-09-29".to_string()),
                supports_reasoning: Some(true),
                supports_tools: Some(true),
            },
            aliases: vec![
                AliasForm {
                    alias: "anthropic/claude-sonnet-4-5".to_string(),
                    model_version: None,
                    source: "derived".to_string(),
                },
                AliasForm {
                    alias: "claude-sonnet-4-5-20250929".to_string(),
                    model_version: Some("20250929".to_string()),
                    source: "derived".to_string(),
                },
            ],
        }
    }

    #[test]
    fn apply_batch_is_idempotent_and_versions_only_on_pricing_change() {
        let (_dir, store) = store();

        let first = store.apply_batch(&[sonnet_upsert()]).expect("first apply");
        assert_eq!(first.cards_added, 1);
        assert_eq!(first.versions_added, 1);
        // Two explicit forms + the slug fallback.
        assert_eq!(first.aliases_added, 3);

        // Identical re-apply: nothing changes — the incremental contract.
        let second = store.apply_batch(&[sonnet_upsert()]).expect("re-apply");
        assert_eq!(second.cards_added, 0);
        assert_eq!(second.versions_added, 0);
        assert_eq!(second.aliases_added, 0);

        // A price change appends version 2; the card row count stays 1.
        let mut repriced = sonnet_upsert();
        repriced.version.input_usd_per_mtok = Some(2.5);
        let third = store.apply_batch(&[repriced]).expect("repriced apply");
        assert_eq!(third.cards_added, 0);
        assert_eq!(third.versions_added, 1);

        let resolved = store
            .resolve("anthropic", "claude-sonnet-4-5")
            .expect("resolve")
            .expect("hit");
        assert_eq!(resolved.version, 2, "latest version is current");
        assert_eq!(resolved.pricing.input_usd_per_mtok, Some(2.5));
        // Non-hashed metadata still rides along.
        assert_eq!(resolved.pricing.output_usd_per_mtok, Some(15.0));
    }

    #[test]
    fn aliases_join_every_string_form_to_the_same_card() {
        let (_dir, store) = store();
        store.apply_batch(&[sonnet_upsert()]).expect("apply");

        let by_slug = store
            .resolve("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .expect("slug resolves");
        let by_full = store
            .resolve("anthropic", "anthropic/claude-sonnet-4-5")
            .unwrap()
            .expect("provider/slug resolves");
        let by_dated = store
            .resolve("anthropic", "claude-sonnet-4-5-20250929")
            .unwrap()
            .expect("dated snapshot resolves");
        assert_eq!(by_slug.model_card_id, by_full.model_card_id);
        assert_eq!(by_slug.model_card_id, by_dated.model_card_id);
        // The dated form carries its version token; the bare slug doesn't.
        assert_eq!(by_dated.model_version.as_deref(), Some("20250929"));
        assert_eq!(by_slug.model_version, None);
        assert_eq!(by_dated.model_provider, "anthropic");

        // Scoped to the API provider: the same string under another
        // provider is a miss.
        assert!(
            store
                .resolve("openai", "claude-sonnet-4-5")
                .unwrap()
                .is_none()
        );
        assert!(store.resolve("anthropic", "claude-nope").unwrap().is_none());
    }

    #[test]
    fn learned_aliases_register_once_and_resolve_afterwards() {
        let (_dir, store) = store();
        store.apply_batch(&[sonnet_upsert()]).expect("apply");
        let card_id = store
            .resolve("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .unwrap()
            .model_card_id;

        let inserted = store
            .insert_learned_alias("anthropic", "claude-sonnet-4-5-wire-form", card_id, None)
            .expect("insert");
        assert!(inserted);
        let again = store
            .insert_learned_alias("anthropic", "claude-sonnet-4-5-wire-form", card_id, None)
            .expect("re-insert");
        assert!(!again, "second registration is a no-op");

        let resolved = store
            .resolve("anthropic", "claude-sonnet-4-5-wire-form")
            .unwrap()
            .expect("learned form resolves");
        assert_eq!(resolved.model_card_id, card_id);
        assert_eq!(resolved.model_provider, "anthropic");

        // A learned alias against a nonexistent card is a named error.
        assert!(
            store
                .insert_learned_alias("anthropic", "orphan", 999_999, None)
                .is_err()
        );
    }

    #[test]
    fn sync_info_roundtrips_and_upserts() {
        let (_dir, store) = store();
        assert!(store.sync_info("models.dev").unwrap().is_none());
        assert!(store.seconds_since_sync("models.dev").unwrap().is_none());

        store
            .record_sync("models.dev", Some("\"etag-1\""), Some("hash-1"))
            .expect("record");
        let info = store.sync_info("models.dev").unwrap().expect("recorded");
        assert_eq!(info.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(info.payload_hash.as_deref(), Some("hash-1"));
        let age = store
            .seconds_since_sync("models.dev")
            .unwrap()
            .expect("age known");
        assert!((0..60).contains(&age), "just-recorded sync is fresh: {age}");

        store
            .record_sync("models.dev", Some("\"etag-2\""), Some("hash-2"))
            .expect("update");
        let info = store.sync_info("models.dev").unwrap().expect("updated");
        assert_eq!(info.etag.as_deref(), Some("\"etag-2\""));
    }

    #[test]
    fn provider_model_count_filters_by_source() {
        let (_dir, store) = store();
        let mut seed_row = sonnet_upsert();
        seed_row.slug = "claude-fable-5".to_string();
        seed_row.source = "seed".to_string();
        seed_row.aliases.clear();
        store
            .apply_batch(&[sonnet_upsert(), seed_row])
            .expect("apply");

        assert_eq!(store.provider_model_count("anthropic", None).unwrap(), 2);
        assert_eq!(
            store
                .provider_model_count("anthropic", Some("models.dev"))
                .unwrap(),
            1,
            "the strictness gate sees only master-list cards"
        );
        assert_eq!(store.provider_model_count("openai", None).unwrap(), 0);
    }

    #[test]
    fn models_for_provider_lists_latest_pricing() {
        let (_dir, store) = store();
        store.apply_batch(&[sonnet_upsert()]).expect("apply");
        let mut repriced = sonnet_upsert();
        repriced.version.output_usd_per_mtok = Some(12.0);
        store.apply_batch(&[repriced]).expect("reprice");

        let all = store.models_for_provider(None).expect("list all");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].version, 2);
        assert_eq!(all[0].pricing.output_usd_per_mtok, Some(12.0));

        let scoped = store
            .models_for_provider(Some("anthropic"))
            .expect("list scoped");
        assert_eq!(scoped.len(), 1);
        assert!(
            store
                .models_for_provider(Some("openai"))
                .expect("list other")
                .is_empty()
        );

        let (cards, versions, aliases) = store.counts().expect("counts");
        assert_eq!(cards, 1);
        assert_eq!(versions, 2);
        assert_eq!(aliases, 3);
    }

    #[test]
    fn capabilities_roundtrip_and_only_hash_when_known() {
        let (_dir, store) = store();
        store.apply_batch(&[sonnet_upsert()]).expect("apply");
        let resolved = store
            .resolve("anthropic", "claude-sonnet-4-5")
            .unwrap()
            .expect("hit");
        assert_eq!(resolved.pricing.supports_reasoning, Some(true));
        assert_eq!(resolved.pricing.supports_tools, Some(true));
        let listing = &store.models_for_provider(Some("anthropic")).unwrap()[0];
        assert_eq!(listing.pricing.supports_reasoning, Some(true));

        // Unknown capabilities hash exactly like the pre-column format —
        // a re-sync of capability-less data must not append a version.
        let mut without = sonnet_upsert().version;
        without.supports_reasoning = None;
        without.supports_tools = None;
        let legacy_hash = fnv_hex(&format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            without.input_usd_per_mtok,
            without.output_usd_per_mtok,
            without.cached_input_usd_per_mtok,
            without.cache_write_usd_per_mtok,
            without.context_window,
            without.max_output_tokens,
        ));
        assert_eq!(without.content_hash(), legacy_hash);
        assert_ne!(sonnet_upsert().version.content_hash(), legacy_hash);

        // Capability data arriving for a previously capability-less card IS
        // a real change: one new version.
        let mut gained = sonnet_upsert();
        gained.version.supports_reasoning = Some(false);
        let counts = store.apply_batch(&[gained]).expect("capability change");
        assert_eq!(counts.versions_added, 1);
    }

    #[test]
    fn open_migrates_a_pre_capability_database_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catalog.db");
        {
            // A database created by an older binary: base schema only (the
            // CATALOG_SCHEMA string IS the original shape; columns arrive
            // via `migrate`), with one card + version already stored.
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(CATALOG_SCHEMA).expect("old schema");
            conn.execute_batch(
                "INSERT INTO model_cards
                     (api_provider, model_provider, slug, source)
                 VALUES ('anthropic', 'anthropic', 'claude-old', 'models.dev');
                 INSERT INTO model_card_versions
                     (model_card_id, version, input_usd_per_mtok, source, content_hash)
                 VALUES (1, 1, 3.0, 'models.dev', 'h');
                 INSERT INTO model_aliases
                     (alias, api_provider, model_provider, model_card_id, source)
                 VALUES ('claude-old', 'anthropic', 'anthropic', 1, 'catalog');",
            )
            .expect("old rows");
        }
        let store = CatalogStore::open(&path).expect("open migrates");
        let resolved = store
            .resolve("anthropic", "claude-old")
            .expect("resolve works post-migration")
            .expect("row survived");
        assert_eq!(resolved.pricing.input_usd_per_mtok, Some(3.0));
        assert_eq!(resolved.pricing.supports_reasoning, None);
        assert_eq!(resolved.pricing.supports_tools, None);
        // Re-open: the migration is idempotent.
        drop(store);
        CatalogStore::open(&path).expect("second open is clean");
    }
}
