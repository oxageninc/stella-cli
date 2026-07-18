//! Shared glob matcher for rule guards (`rules.rs`) and hook matchers
//! (`hooks.rs`) — ports `matchGlob` from
//! `packages/mcp-config/src/permissions.ts`: `*` is the only wildcard,
//! every other character is a literal. No `regex` dependency: this crate
//! has none, and the language is small enough that a hand-written segment
//! matcher is clearer than pulling one in for one wildcard character.

/// `true` if `value` matches `pattern`. Exact string equality when
/// `pattern` has no `*`; `"*"` alone always matches. Otherwise the pattern
/// is split on `*` into literal segments that must appear in `value`, in
/// order — the first segment anchored to the start (unless the pattern
/// itself starts with `*`), the last anchored to the end (unless it ends
/// with `*`), everything in between found anywhere after the previous
/// match. This is the classic single-wildcard glob algorithm, and it is
/// exactly what the TS source's `pattern.replace(/\*/g, ".*")` regex
/// compiles down to.
pub(crate) fn match_glob(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    if pattern == "*" {
        return true;
    }

    let segments: Vec<&str> = pattern.split('*').collect();
    let last_idx = segments.len() - 1;
    let mut pos = 0usize;

    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            let Some(rest) = value.get(pos..) else {
                return false;
            };
            if !rest.starts_with(seg) {
                return false;
            }
            pos += seg.len();
        } else if i == last_idx {
            let Some(rest) = value.get(pos..) else {
                return false;
            };
            if !rest.ends_with(seg) {
                return false;
            }
        } else {
            let Some(rest) = value.get(pos..) else {
                return false;
            };
            match rest.find(seg) {
                Some(offset) => pos += offset + seg.len(),
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_with_no_wildcard() {
        assert!(match_glob("bash", "bash"));
        assert!(!match_glob("bash", "write_file"));
    }

    #[test]
    fn bare_star_matches_anything() {
        assert!(match_glob("*", "anything"));
        assert!(match_glob("*", ""));
    }

    #[test]
    fn trailing_star_matches_prefix() {
        assert!(match_glob("git push*", "git push --force origin main"));
        assert!(!match_glob("git push*", "git pull"));
    }

    #[test]
    fn leading_star_matches_suffix() {
        assert!(match_glob("*.md", "notes.md"));
        assert!(!match_glob("*.md", "notes.txt"));
    }

    #[test]
    fn star_in_the_middle_requires_both_ends() {
        assert!(match_glob(
            "packages/database/migrations/*-applied/**",
            "packages/database/migrations/0001-applied/up.sql"
        ));
        assert!(!match_glob(
            "packages/database/migrations/*-applied/**",
            "src/app.ts"
        ));
    }

    #[test]
    fn double_star_behaves_like_a_single_wildcard() {
        // `**` is two adjacent `*`s, each independently meaning "any
        // characters" — not a special "zero-or-more path segments" glob
        // token (unlike fd/rg/gitignore's `**`). It matches anything a
        // single `*` would…
        assert!(match_glob("a/**/b", "a/x/y/b"));
        assert_eq!(
            match_glob("a/**/b", "a/x/y/b"),
            match_glob("a/*/b", "a/x/y/b")
        );
        // …and, less intuitively but consistently, does NOT match "a/b":
        // the pattern's literal segments are "a/" and "/b", and matching
        // requires an actual "/" character before the final "b" — "a/b"
        // only has one slash total, already consumed by the first segment.
        // A single `*` has exactly the same requirement, hence "behaves
        // like a single wildcard" — both fail here, not both succeed.
        assert!(!match_glob("a/**/b", "a/b"));
        assert_eq!(match_glob("a/**/b", "a/b"), match_glob("a/*/b", "a/b"));
    }

    #[test]
    fn empty_pattern_only_matches_empty_value() {
        assert!(match_glob("", ""));
        assert!(!match_glob("", "x"));
    }
}
