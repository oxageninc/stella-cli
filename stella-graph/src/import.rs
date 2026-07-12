//! Import edges (`file → imported module/file`) and the resolution of
//! relative specifiers to actual files.
//!
//! The spec singles this out (`03-plan.md` Phase 3 item 3): "fix the known
//! thin Python import-edge resolution rather than porting it". So Python
//! **relative** imports (`from . import x`, `from ..pkg import y`) resolve to
//! real files, and TS/JS relative specifiers (`./x`, `../y`) resolve through
//! the usual extension/`index.*` candidate ladder. Bare package specifiers
//! (`react`, `os.path`, a Rust `use` path) are recorded **unresolved** — the
//! edge is preserved even when its target is outside the indexed tree.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// How an import specifier was resolved, stored in `code_graph_imports.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// A relative specifier we attempt to resolve to a file in the tree.
    Relative,
    /// A bare package specifier (`react`, `@scope/pkg`).
    Bare,
    /// An absolute language import we do not resolve to a file (Python
    /// `import os.path`, a Rust `use` path).
    Absolute,
}

impl ImportKind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            ImportKind::Relative => "relative",
            ImportKind::Bare => "bare",
            ImportKind::Absolute => "absolute",
        }
    }
}

/// A resolved (or deliberately unresolved) import edge out of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportEdge {
    /// The specifier as written (or, for `from . import x`, reconstructed as
    /// `.x`) — a human-readable label, never a raw id.
    pub specifier: String,
    /// The resolved target, as a forward-slash path relative to the
    /// workspace root; `None` when the specifier is bare/absolute or the
    /// target is not a file inside the tree.
    pub to_path: Option<String>,
    pub kind: ImportKind,
}

impl ImportEdge {
    fn unresolved(specifier: String, kind: ImportKind) -> Self {
        ImportEdge {
            specifier,
            to_path: None,
            kind,
        }
    }
}

/// The language-specific raw import intent [`crate::parse`] extracts, before
/// filesystem resolution (which needs the importing file's path + the root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportSpec {
    /// TS/JS relative specifier, e.g. `./foo`, `../bar/baz`, `./x.js`.
    TsRelative { specifier: String },
    /// TS/JS bare package specifier, e.g. `react`, `@stella/ui`.
    Bare { specifier: String },
    /// Python `import a`, `import a.b` — absolute, unresolved.
    PyAbsolute { specifier: String },
    /// Python `from <rel> import …`. `level` = number of leading dots (1 =
    /// current package); `module` = the dotted path after the dots, if any;
    /// `names` = imported names (used only when `module` is `None`, i.e.
    /// `from . import x, y`); `text` = the relative prefix as written.
    PyRelative {
        level: usize,
        module: Option<String>,
        names: Vec<String>,
        text: String,
    },
    /// A Rust `use` path — recorded unresolved (module→file resolution is
    /// out of scope, see [`crate::queries::RUST_IMPORTS`]).
    RustUse { specifier: String },
}

/// Resolve a file's raw import specifiers to edges. `root` must already be
/// canonicalized (see [`crate::graph::CodeGraph`]); `file_abs` is the
/// importing file's absolute path.
pub(crate) fn resolve(specs: Vec<ImportSpec>, root: &Path, file_abs: &Path) -> Vec<ImportEdge> {
    let file_dir = file_abs.parent().unwrap_or(root);
    let mut edges = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec {
            ImportSpec::Bare { specifier } => {
                edges.push(ImportEdge::unresolved(specifier, ImportKind::Bare));
            }
            ImportSpec::PyAbsolute { specifier } | ImportSpec::RustUse { specifier } => {
                edges.push(ImportEdge::unresolved(specifier, ImportKind::Absolute));
            }
            ImportSpec::TsRelative { specifier } => {
                let to_path = resolve_ts_relative(&specifier, file_dir, root);
                edges.push(ImportEdge {
                    specifier,
                    to_path,
                    kind: ImportKind::Relative,
                });
            }
            ImportSpec::PyRelative {
                level,
                module,
                names,
                text,
            } => resolve_py_relative(level, module, names, text, file_dir, root, &mut edges),
        }
    }
    edges
}

/// Append the base directory of a Python relative import: the current
/// package is the file's own directory (`level` 1), each extra dot climbs one
/// package (`09-lessons-learned` has no entry here — this is the spec's
/// explicitly-requested fix). Returns `None` if the dots climb above the FS
/// root.
fn py_base_dir(file_dir: &Path, level: usize) -> Option<PathBuf> {
    let mut base = file_dir.to_path_buf();
    for _ in 0..level.saturating_sub(1) {
        base = base.parent()?.to_path_buf();
    }
    Some(base)
}

fn resolve_py_relative(
    level: usize,
    module: Option<String>,
    names: Vec<String>,
    text: String,
    file_dir: &Path,
    root: &Path,
    edges: &mut Vec<ImportEdge>,
) {
    let Some(base) = py_base_dir(file_dir, level) else {
        edges.push(ImportEdge::unresolved(text, ImportKind::Relative));
        return;
    };
    match module {
        // `from .pkg.sub import name` → the module is `pkg/sub`; the imported
        // names live inside it, so the single edge points at that module.
        Some(module) => {
            let mut target = base;
            for part in module.split('.') {
                target = target.join(part);
            }
            edges.push(ImportEdge {
                specifier: text,
                to_path: resolve_py_module(&target, root),
                kind: ImportKind::Relative,
            });
        }
        // `from . import a, b` → each imported name is itself a submodule of
        // the current package; one edge per name.
        None => {
            if names.is_empty() {
                edges.push(ImportEdge::unresolved(text, ImportKind::Relative));
                return;
            }
            for name in names {
                let target = base.join(&name);
                edges.push(ImportEdge {
                    specifier: format!("{text}{name}"),
                    to_path: resolve_py_module(&target, root),
                    kind: ImportKind::Relative,
                });
            }
        }
    }
}

/// A Python module path resolves to `<base>.py` or `<base>/__init__.py`.
fn resolve_py_module(base: &Path, root: &Path) -> Option<String> {
    let mut file = base.to_path_buf();
    file.set_extension("py");
    if let Some(rel) = existing_within_root(&file, root) {
        return Some(rel);
    }
    let pkg = base.join("__init__.py");
    existing_within_root(&pkg, root)
}

/// The TS/JS relative-specifier candidate ladder: an explicit recognized
/// extension is tried first (with the common ESM `.js`→`.ts` rewrite), then
/// the bare specifier gains each source extension, then `index.*` inside a
/// directory of that name. First existing file inside the root wins.
fn resolve_ts_relative(specifier: &str, file_dir: &Path, root: &Path) -> Option<String> {
    const EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let joined = file_dir.join(specifier);
    let mut candidates: Vec<PathBuf> = Vec::new();

    let has_known_ext = joined
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTS.contains(&e) || e == "json")
        .unwrap_or(false);

    if has_known_ext {
        candidates.push(joined.clone());
        // `import './x.js'` in TS source usually resolves to `./x.ts`.
        match joined.extension().and_then(|e| e.to_str()) {
            Some("js") => {
                candidates.push(joined.with_extension("ts"));
                candidates.push(joined.with_extension("tsx"));
            }
            Some("jsx") => candidates.push(joined.with_extension("tsx")),
            _ => {}
        }
    } else {
        for ext in EXTS {
            candidates.push(append_ext(&joined, ext));
        }
        for ext in EXTS {
            candidates.push(joined.join(format!("index.{ext}")));
        }
    }

    candidates
        .into_iter()
        .find_map(|c| existing_within_root(&c, root))
}

/// Append `.ext` to a path *without* replacing an existing extension (unlike
/// `Path::with_extension`), so `./x` → `./x.ts`.
fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(".");
    os.push(ext);
    PathBuf::from(os)
}

/// If `candidate` is an existing regular file inside `root`, return its
/// forward-slash path relative to `root`; otherwise `None`. Canonicalization
/// resolves any `..` segments and symlinks so the root-jail check is honest
/// (`02-architecture.md` §8 workspace-root jail).
fn existing_within_root(candidate: &Path, root: &Path) -> Option<String> {
    if !candidate.is_file() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    let rel = canonical.strip_prefix(root).ok()?;
    Some(rel_to_slash(rel))
}

/// Normalize a relative path to forward slashes for stable, cross-platform
/// storage in the `to_path` column.
pub(crate) fn rel_to_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
