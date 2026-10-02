//! Source positions without spans.
//!
//! Every token is a `&str` slice of the original source. We recover its byte
//! offset from the slice pointer, and line and column from the offset. This is
//! only done when a diagnostic needs a position, so the hot path carries no span data.

/// Byte offset of `part` within `src`.
///
/// Panics if `part` is not a sub-slice of `src`.
#[inline]
pub fn offset_of(src: &str, part: &str) -> usize {
    let off = (part.as_ptr() as usize).wrapping_sub(src.as_ptr() as usize);
    assert!(
        off <= src.len() && part.len() <= src.len() - off,
        "slice is not part of the source"
    );
    off
}

/// 1-based line and column of byte offset `off` in `src`.
///
/// Lines end at `\n`, `\r\n` or a lone `\r`. The column counts characters
/// (not bytes) from the start of the line, and a tab counts as one character.
pub fn line_col_at(src: &str, off: usize) -> (usize, usize) {
    let bytes = &src.as_bytes()[..off];
    let mut line = 1;
    let mut line_start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let ends_line = b == b'\n' || (b == b'\r' && src.as_bytes().get(i + 1) != Some(&b'\n'));
        if ends_line {
            line += 1;
            line_start = i + 1;
        }
    }
    let col = src[line_start..off].chars().count() + 1;
    (line, col)
}

/// 1-based line and column of the start of `part` within `src`.
#[inline]
pub fn line_col(src: &str, part: &str) -> (usize, usize) {
    line_col_at(src, offset_of(src, part))
}

/// 1-based line and column of the position just past the end of `part`.
#[inline]
pub fn line_col_end(src: &str, part: &str) -> (usize, usize) {
    line_col_at(src, offset_of(src, part) + part.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions() {
        let src = "ab\ncd\r\nef\rgh";
        assert_eq!(line_col(src, &src[0..1]), (1, 1));
        assert_eq!(line_col(src, &src[4..5]), (2, 2));
        assert_eq!(line_col(src, &src[7..8]), (3, 1));
        assert_eq!(line_col(src, &src[10..11]), (4, 1));
        assert_eq!(line_col_end(src, &src[10..12]), (4, 3));
    }

    #[test]
    fn columns_count_chars() {
        let src = "/* é */ x";
        let x = &src[src.len() - 1..];
        assert_eq!(line_col(src, x), (1, 9));
    }

    #[test]
    #[should_panic]
    fn foreign_slice_panics() {
        let other = String::from("x");
        offset_of("abc", &other);
    }
}
