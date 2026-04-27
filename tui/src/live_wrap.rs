use unicode_width::UnicodeWidthChar;

/// Take a prefix of `text` whose visible width is at most `max_cols`.
/// Returns (prefix, suffix, prefix_width).
pub fn take_prefix_by_width(text: &str, max_cols: usize) -> (String, &str, usize) {
    if max_cols == 0 || text.is_empty() {
        return (String::new(), text, 0);
    }
    let mut cols = 0usize;
    let mut end_idx = 0usize;
    for (idx, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cols.saturating_add(ch_width) > max_cols {
            break;
        }
        cols += ch_width;
        end_idx = idx + ch.len_utf8();
        if cols == max_cols {
            break;
        }
    }
    let prefix = text[..end_idx].to_string();
    let suffix = &text[end_idx..];
    (prefix, suffix, cols)
}
