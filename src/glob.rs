//! Glob matching for rule `paths:` patterns: `?`, `*`, `**`, and `{a,b}`
//! brace alternation.

use std::collections::HashSet;

use crate::pathutil::{strip_dot_slash, to_posix};

/// Match a POSIX-style path glob against a candidate path.
///
/// Supports `?` (any single character except `/`), `*` (any run within one path
/// segment), `**` (any run across segments, including `/`), and `{a,b}` brace
/// alternation, expanded to its alternatives before matching so the candidate
/// matches when any expansion does. Both inputs are normalized to forward
/// slashes with a leading `./` stripped.
pub(crate) fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let candidate = to_posix(candidate);
    expand_braces(&to_posix(pattern)).iter().any(|expanded| {
        let expanded = strip_dot_slash(expanded);
        let mut failed = HashSet::new();
        glob_matches_from(expanded.as_bytes(), 0, candidate.as_bytes(), 0, &mut failed)
    })
}

/// Expand brace alternations such as `a.{x,y}` into the concrete patterns
/// `["a.x", "a.y"]`, taking the cartesian product across multiple groups.
///
/// Follows shell/native semantics: a `{...}` group without a top-level comma
/// (e.g. `{x}`) is left literal, and an unbalanced brace disables expansion.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = matching_brace(pattern, open) else {
        return vec![pattern.to_owned()];
    };

    let alternatives = split_top_level_commas(&pattern[open + 1..close]);
    if alternatives.len() < 2 {
        return vec![pattern.to_owned()];
    }

    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let mut expanded = Vec::new();
    for alternative in alternatives {
        let branch = format!("{prefix}{}{suffix}", alternative.trim());
        for expansion in expand_braces(&branch) {
            if !expanded.contains(&expansion) {
                expanded.push(expansion);
            }
        }
    }

    expanded
}

/// Byte offset of the `}` closing the `{` at `open`, accounting for nested
/// braces, or `None` when the brace is unbalanced.
fn matching_brace(pattern: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in pattern[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }

    None
}

/// Split `text` on commas that sit outside any quotes or `{}`/`[]` nesting, so
/// a list separator is never confused with a comma inside a quoted value or a
/// `{a,b}` brace group.
pub(crate) fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0;

    for (offset, character) in text.char_indices() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }

    parts.push(&text[start..]);
    parts
}

/// Recursive backtracking matcher backing [`glob_matches`].
///
/// `failed` memoizes `(pattern_index, candidate_index)` pairs already known not
/// to match, which bounds the otherwise exponential backtracking of overlapping
/// `*`/`**` wildcards to polynomial time.
fn glob_matches_from(
    pattern: &[u8],
    pattern_index: usize,
    candidate: &[u8],
    candidate_index: usize,
    failed: &mut HashSet<(usize, usize)>,
) -> bool {
    if failed.contains(&(pattern_index, candidate_index)) {
        return false;
    }

    let matched = if pattern_index == pattern.len() {
        candidate_index == candidate.len()
    } else if pattern[pattern_index..].starts_with(b"**/") {
        glob_matches_from(
            pattern,
            pattern_index + 3,
            candidate,
            candidate_index,
            failed,
        ) || candidate[candidate_index..]
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .map(|(offset, _)| candidate_index + offset + 1)
            .any(|next_index| {
                glob_matches_from(pattern, pattern_index + 3, candidate, next_index, failed)
            })
    } else if pattern[pattern_index..].starts_with(b"**") {
        (candidate_index..=candidate.len()).any(|next_index| {
            glob_matches_from(pattern, pattern_index + 2, candidate, next_index, failed)
        })
    } else if pattern[pattern_index] == b'*' {
        let segment_end = candidate[candidate_index..]
            .iter()
            .position(|byte| *byte == b'/')
            .map_or(candidate.len(), |offset| candidate_index + offset);
        (candidate_index..=segment_end).any(|next_index| {
            glob_matches_from(pattern, pattern_index + 1, candidate, next_index, failed)
        })
    } else if pattern[pattern_index] == b'?' {
        candidate
            .get(candidate_index)
            .is_some_and(|candidate_char| *candidate_char != b'/')
            && glob_matches_from(
                pattern,
                pattern_index + 1,
                candidate,
                candidate_index + 1,
                failed,
            )
    } else {
        candidate
            .get(candidate_index)
            .is_some_and(|candidate_char| *candidate_char == pattern[pattern_index])
            && glob_matches_from(
                pattern,
                pattern_index + 1,
                candidate,
                candidate_index + 1,
                failed,
            )
    };

    if !matched {
        failed.insert((pattern_index, candidate_index));
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_literal_matches_identical_path() {
        assert!(glob_matches("src/app.ts", "src/app.ts"));
    }

    #[test]
    fn glob_literal_rejects_a_different_path() {
        assert!(!glob_matches("src/app.ts", "src/app.js"));
    }

    #[test]
    fn glob_single_star_stays_within_one_segment() {
        assert!(glob_matches("src/*.css", "src/stage.css"));
    }

    #[test]
    fn glob_single_star_rejects_crossing_a_slash() {
        assert!(!glob_matches("src/*.css", "src/styles/stage.css"));
    }

    #[test]
    fn glob_double_star_crosses_segments() {
        assert!(glob_matches("src/**/*.css", "src/a/b/stage.css"));
    }

    #[test]
    fn glob_double_star_matches_zero_intermediate_segments() {
        assert!(glob_matches("src/**/*.css", "src/stage.css"));
    }

    #[test]
    fn glob_leading_double_star_matches_any_prefix() {
        assert!(glob_matches("**/tokens.css", "src/styles/tokens.css"));
    }

    #[test]
    fn glob_question_mark_matches_one_non_slash_character() {
        assert!(glob_matches("a?c.ts", "abc.ts"));
    }

    #[test]
    fn glob_question_mark_rejects_a_slash() {
        assert!(!glob_matches("a?c", "a/c"));
    }

    #[test]
    fn glob_brace_matches_a_listed_extension() {
        assert!(glob_matches("src/**/*.{css,astro,svelte}", "src/a/b.css"));
    }

    #[test]
    fn glob_brace_matches_a_middle_alternative() {
        assert!(glob_matches("src/**/*.{css,astro,svelte}", "src/a/b.astro"));
    }

    #[test]
    fn glob_brace_matches_a_trailing_alternative() {
        assert!(glob_matches(
            "src/**/*.{css,astro,svelte}",
            "src/a/b.svelte"
        ));
    }

    #[test]
    fn glob_brace_rejects_an_unlisted_extension() {
        assert!(!glob_matches("src/**/*.{css,astro}", "src/a/b.ts"));
    }

    #[test]
    fn glob_brace_expands_two_groups_independently() {
        assert!(glob_matches("{src,lib}/**/*.{ts,tsx}", "lib/a/b.tsx"));
    }
}
