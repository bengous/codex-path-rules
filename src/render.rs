//! Rendering of matched rules as `<rule>…</rule>` blocks within the injection
//! budget.

use crate::rules::Rule;

/// Maximum number of characters emitted for a single rule body, truncation
/// notice included.
const MAX_RULE_CHARS: usize = 6000;
/// Maximum number of characters emitted across all rules in one injection.
const MAX_BATCH_CHARS: usize = 12000;
/// Appended in place of content dropped by [`truncate`]; counted within the
/// per-rule budget.
const TRUNCATION_NOTICE: &str =
    "\n[Rule truncated. Read the rule file for full details if needed.]";
/// Separator between rendered rule blocks.
const BLOCK_JOINER: &str = "\n\n";

/// One rendered injection batch: the `additionalContext` text plus the keys of
/// the rules it actually contains.
#[derive(Debug)]
pub(crate) struct RenderedBatch {
    pub(crate) context: String,
    pub(crate) emitted_keys: Vec<String>,
}

/// Render `candidates` as `<rule>…</rule>` blocks within [`MAX_BATCH_CHARS`].
///
/// Each body is capped at [`MAX_RULE_CHARS`]. A candidate whose block no
/// longer fits the remaining batch budget is *deferred*, not truncated harder:
/// it is left out of `emitted_keys`, so the caller keeps it unmarked and a
/// later matching tool call injects it with a fresh budget. Later, smaller
/// candidates may still fit and are not blocked by a deferred one. Because a
/// single capped block always fits an empty batch, the first candidate is
/// always emitted.
pub(crate) fn render_rules(candidates: &[Rule]) -> RenderedBatch {
    let mut blocks: Vec<String> = Vec::new();
    let mut emitted_keys = Vec::new();
    let mut remaining = MAX_BATCH_CHARS;

    for rule in candidates {
        let content = truncate(&neutralize_rule_closer(&rule.content), MAX_RULE_CHARS);
        let block = format!("<rule>\n{content}\n</rule>");
        let joiner_cost = if blocks.is_empty() {
            0
        } else {
            BLOCK_JOINER.chars().count()
        };
        let cost = block.chars().count() + joiner_cost;

        if cost > remaining {
            continue;
        }

        remaining -= cost;
        blocks.push(block);
        emitted_keys.push(rule.key.clone());
    }

    RenderedBatch {
        context: blocks.join(BLOCK_JOINER),
        emitted_keys,
    }
}

/// Rewrite literal `</rule>` closers so a rule body cannot terminate its
/// wrapper block early. Nothing else is escaped: rule bodies routinely contain
/// code with `<`, `>`, and `&` that must reach the model verbatim.
fn neutralize_rule_closer(value: &str) -> String {
    value.replace("</rule>", "<\\/rule>")
}

/// Truncate `text` to at most `max_chars` characters, truncation notice
/// included, so callers can budget on the returned length.
///
/// `max_chars` must exceed the notice length; [`MAX_RULE_CHARS`] does.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let keep = max_chars.saturating_sub(TRUNCATION_NOTICE.chars().count());
    let prefix = text.chars().take(keep).collect::<String>();
    format!("{prefix}{TRUNCATION_NOTICE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(key: &str, content: String) -> Rule {
        Rule {
            key: key.to_owned(),
            paths: None,
            content,
        }
    }

    #[test]
    fn truncate_keeps_short_text_unchanged() {
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn truncate_result_stays_within_the_limit() {
        let long = "a".repeat(MAX_RULE_CHARS + 1000);
        let truncated = truncate(&long, MAX_RULE_CHARS);
        assert_eq!(truncated.chars().count(), MAX_RULE_CHARS);
        assert!(truncated.ends_with(TRUNCATION_NOTICE));
    }

    #[test]
    fn neutralize_rewrites_a_literal_closing_tag() {
        assert_eq!(
            neutralize_rule_closer("before</rule>after"),
            "before<\\/rule>after"
        );
    }

    #[test]
    fn render_keeps_code_characters_verbatim() {
        let batch = render_rules(&[rule("k", "Use `Vec<String>` & friends.".to_owned())]);
        assert_eq!(
            batch.context,
            "<rule>\nUse `Vec<String>` & friends.\n</rule>"
        );
    }

    #[test]
    fn render_emits_every_rule_that_fits_the_batch() {
        let batch = render_rules(&[rule("a", "one".to_owned()), rule("b", "two".to_owned())]);
        assert_eq!(batch.emitted_keys, ["a", "b"]);
        assert_eq!(
            batch.context,
            "<rule>\none\n</rule>\n\n<rule>\ntwo\n</rule>"
        );
    }

    #[test]
    fn render_defers_a_rule_that_overflows_the_batch() {
        let big = "x".repeat(MAX_RULE_CHARS);
        let batch = render_rules(&[rule("a", big.clone()), rule("b", big)]);
        assert_eq!(
            batch.emitted_keys,
            ["a"],
            "the second rule should be deferred, not dropped into the injected state"
        );
    }

    #[test]
    fn render_still_emits_a_smaller_rule_after_a_deferred_one() {
        let big = "x".repeat(MAX_RULE_CHARS);
        let batch = render_rules(&[
            rule("a", big.clone()),
            rule("b", big),
            rule("c", "small".to_owned()),
        ]);
        assert_eq!(batch.emitted_keys, ["a", "c"]);
    }

    #[test]
    fn render_caps_a_single_rule_at_the_per_rule_budget() {
        let batch = render_rules(&[rule("a", "y".repeat(MAX_RULE_CHARS * 2))]);
        let wrapper_chars = "<rule>\n\n</rule>".chars().count();
        assert_eq!(
            batch.context.chars().count(),
            MAX_RULE_CHARS + wrapper_chars
        );
        assert!(batch.context.contains(TRUNCATION_NOTICE));
    }
}
