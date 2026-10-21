//! Typography-tolerant entry matching for `memory` action `patch`.
//!
//! Ported from the original's `_normalize_for_match` / `_find_unique_match`
//! / `_patch_entry` (`tools/memory_tool_store.py`) via the reference port's
//! batch-8 work. Models often retype an entry with straight quotes for curly
//! quotes, `-` for em dashes, or re-wrapped lines; the folded matching below
//! tolerates that.
//!
//! Used by [`crate::backends::memory::FileMemoryBackend::patch`].

use hermes_core::ToolError;

/// Fold typography for matching: all quote/backtick variants -> `'`, all
/// dash variants -> `-`, and whitespace runs -> a single space.
fn normalize_for_match(text: &str) -> String {
    let folded: String = text
        .replace("\\n", " ")
        .chars()
        .map(|c| match c {
            '"' | '`' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{201C}'
            | '\u{201D}' | '\u{201E}' | '\u{201F}' => '\'',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            _ => c,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Index of the entry addressed by `old_text`, or an error when none or
/// several *distinct* entries match.
///
/// - An entry exactly equal to `old_text` (whole-entry exact) always wins
///   outright — substring matches are only considered when no entry is
///   exactly equal (so a short entry stays addressable even when its text is
///   contained in a longer entry).
/// - Duplicate identical entries are safe (the first wins); several *distinct*
///   matches are ambiguous, like the original.
pub fn locate_entry_index(entries: &[String], old_text: &str) -> Result<usize, ToolError> {
    if old_text.trim().is_empty() {
        return Err(ToolError::InvalidParams(
            "old_text cannot be empty for action='patch'.".to_string(),
        ));
    }
    // Priority 1: whole-entry exact.
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.as_str() == old_text)
        .map(|(i, _)| i)
        .collect();
    let matches: Vec<usize> = if !exact.is_empty() {
        exact
    } else {
        substring_matches(entries, old_text)
    };
    // Identical duplicates are safe (first wins); distinct entries -> ambiguous.
    let distinct: std::collections::HashSet<&str> =
        matches.iter().map(|&i| entries[i].as_str()).collect();
    if distinct.len() > 1 {
        return Err(ToolError::ExecutionFailed(format!(
            "old_text matched {} different entries — be more specific.",
            distinct.len()
        )));
    }
    matches.into_iter().next().ok_or_else(|| {
        ToolError::ExecutionFailed(format!(
            "No memory entry matched '{old_text}'. Copy old_text from the intended entry."
        ))
    })
}

fn substring_matches(entries: &[String], old_text: &str) -> Vec<usize> {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.contains(old_text))
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let needle = normalize_for_match(old_text);
    // Punctuation/whitespace alone would match every entry — reject it.
    if !needle.chars().any(|c| c.is_alphanumeric()) {
        return vec![];
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| normalize_for_match(e).contains(needle.as_str()))
        .map(|(i, _)| i)
        .collect()
}

/// Replace the single `old_text` span inside `entry_text` with `new_text`
/// (parity with the original's `_patch_entry`):
/// - a span occurring more than once is rejected (ambiguous) rather than
///   edited everywhere;
/// - a result identical to the input is rejected (no-op).
pub fn splice_patch_span(
    entry_text: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, ToolError> {
    let count = span_occurrences(entry_text, old_text);
    if count > 1 {
        return Err(ToolError::ExecutionFailed(format!(
            "'{old_text}' occurs {count} times in the entry; patch edits a single \
             span. Include surrounding text so old_text occurs exactly once."
        )));
    }
    let patched = entry_text.replacen(old_text, new_text, 1);
    if patched == entry_text {
        return Err(ToolError::ExecutionFailed(format!(
            "Patch changed nothing (no-op): '{old_text}' not found byte-exact in the entry."
        )));
    }
    Ok(patched)
}

/// Occurrences of `span` in `text`, including overlapping ones
/// (the original: '00' occurs twice in '1000').
///
/// Iterates char boundaries, not byte indices: slicing a multibyte string
/// at a non-char-boundary byte index would panic.
fn span_occurrences(text: &str, span: &str) -> usize {
    if span.is_empty() || span.len() > text.len() {
        return 0;
    }
    text.char_indices()
        .filter(|(i, _)| text[*i..].starts_with(span))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_match_wins_over_substring() {
        let e = entries(&["blue sky", "the blue sky is nice"]);
        assert_eq!(locate_entry_index(&e, "blue sky").unwrap(), 0);
    }

    #[test]
    fn folded_match_tolerates_typography() {
        let e = entries(&["the “quick” brown—fox"]);
        // Straight quotes + hyphen for curly quotes + em dash, re-wrapped.
        assert_eq!(
            locate_entry_index(&e, "the \"quick\"\nbrown-fox").unwrap(),
            0
        );
    }

    #[test]
    fn folded_match_ignores_whitespace_runs() {
        let e = entries(&["first line  second   line"]);
        assert_eq!(locate_entry_index(&e, "first line second line").unwrap(), 0);
    }

    #[test]
    fn distinct_matches_are_ambiguous() {
        let e = entries(&["alpha beta", "beta gamma"]);
        assert!(locate_entry_index(&e, "beta").is_err());
    }

    #[test]
    fn duplicate_identical_entries_first_wins() {
        let e = entries(&["same", "same"]);
        assert_eq!(locate_entry_index(&e, "same").unwrap(), 0);
    }

    #[test]
    fn no_match_errors() {
        let e = entries(&["hello"]);
        assert!(locate_entry_index(&e, "world").is_err());
    }

    #[test]
    fn empty_old_text_rejected() {
        let e = entries(&["hello"]);
        assert!(matches!(
            locate_entry_index(&e, "  "),
            Err(ToolError::InvalidParams(_))
        ));
    }

    #[test]
    fn splice_replaces_single_span() {
        let out = splice_patch_span("the sky is blue", "blue", "green").unwrap();
        assert_eq!(out, "the sky is green");
    }

    #[test]
    fn splice_rejects_multiple_occurrences() {
        assert!(splice_patch_span("a b a", "a", "z").is_err());
    }

    #[test]
    fn splice_counts_overlapping_occurrences() {
        assert!(splice_patch_span("1000", "00", "x").is_err());
    }

    #[test]
    fn span_occurrences_handles_multibyte_text() {
        // Byte-index iteration would panic slicing inside '“' (3 bytes).
        let ok = splice_patch_span("the “quick” brown fox", "brown", "red").unwrap();
        assert_eq!(ok, "the “quick” red fox");
        assert!(splice_patch_span("“a” “a”", "“a”", "b").is_err());
    }

    #[test]
    fn splice_rejects_noop() {
        assert!(splice_patch_span("abc", "zzz", "q").is_err());
    }

    #[test]
    fn splice_keeps_rest_of_entry() {
        let out = splice_patch_span("prefix TARGET suffix", "TARGET", "NEW").unwrap();
        assert_eq!(out, "prefix NEW suffix");
    }
}
