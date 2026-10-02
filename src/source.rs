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

/// Index of a buffer in a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufId(pub u32);

/// Where a buffer's text came from.
#[derive(Clone, Debug)]
pub enum Origin {
    /// A source file. `name` is the path as it should appear in diagnostics and
    /// `` `__FILE__ ``. `parent` is the file that included it.
    File { name: String, parent: Option<BufId> },
    /// Text made by the preprocessor (a macro expansion, a stringification, a
    /// normalised comment). Positions inside it are reported at `site`, a byte
    /// offset in buffer `in_buf`.
    Derived { in_buf: BufId, site: usize },
    /// Text from the command line, such as `-D` values.
    CommandLine,
}

/// A `` `line `` directive: from physical line `phys` on, lines are numbered
/// from `logical` and the file is called `name`.
#[derive(Clone, Debug)]
struct LineRemap {
    phys: usize,
    logical: usize,
    name: String,
}

struct Buffer {
    text: Box<str>,
    origin: Origin,
    /// Byte offsets where each line starts. Built on first use.
    line_starts: std::cell::OnceCell<Vec<usize>>,
    remaps: Vec<LineRemap>,
}

/// A resolved source position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The file buffer the position resolves to.
    pub file: BufId,
    /// The file name, after any `` `line `` directive.
    pub name: String,
    /// 1-based line, after any `` `line `` directive.
    pub line: usize,
    /// 1-based column, in characters.
    pub col: usize,
}

/// An append-only arena of source texts.
///
/// Text added here lives as long as the map, so tokens can borrow it as plain
/// `&str` for the map's lifetime. Each buffer records its origin, so any slice
/// can be traced back to a file position when a diagnostic needs one.
#[derive(Default)]
pub struct SourceMap {
    buffers: std::cell::RefCell<Vec<Buffer>>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a buffer and borrow its text for the life of the map.
    pub fn add(&self, text: String, origin: Origin) -> (BufId, &str) {
        let text = text.into_boxed_str();
        let ptr: *const str = &*text;
        let mut bufs = self.buffers.borrow_mut();
        let id = BufId(bufs.len() as u32);
        bufs.push(Buffer {
            text,
            origin,
            line_starts: Default::default(),
            remaps: Vec::new(),
        });
        // SAFETY: the boxed text is never moved, mutated or dropped while the
        // map is alive, because buffers are only ever appended.
        (id, unsafe { &*ptr })
    }

    /// Add derived text whose position is reported at `site`.
    pub fn derive<'a>(&'a self, text: String, site: &str) -> &'a str {
        let origin = match self.find(site) {
            Some((in_buf, off)) => Origin::Derived { in_buf, site: off },
            None => Origin::CommandLine,
        };
        self.add(text, origin).1
    }

    /// The buffer containing `s` and the offset of `s` within it.
    pub fn find(&self, s: &str) -> Option<(BufId, usize)> {
        let p = s.as_ptr() as usize;
        let bufs = self.buffers.borrow();
        bufs.iter().enumerate().rev().find_map(|(i, b)| {
            let start = b.text.as_ptr() as usize;
            (p >= start && p + s.len() <= start + b.text.len())
                .then(|| (BufId(i as u32), p - start))
        })
    }

    pub fn origin(&self, id: BufId) -> Origin {
        self.buffers.borrow()[id.0 as usize].origin.clone()
    }

    /// The text of a buffer.
    pub fn text(&self, id: BufId) -> &str {
        let ptr: *const str = &*self.buffers.borrow()[id.0 as usize].text;
        // SAFETY: as in `add`.
        unsafe { &*ptr }
    }

    /// Record a `` `line `` directive: physical line `phys` of file `id` is
    /// logical line `logical` of file `name`.
    pub fn remap_lines(&self, id: BufId, phys: usize, logical: usize, name: String) {
        self.buffers.borrow_mut()[id.0 as usize]
            .remaps
            .push(LineRemap {
                phys,
                logical,
                name,
            });
    }

    /// Resolve a slice to a file position, following derived text back to the
    /// place it was made. Returns `None` for text not in the map (for example
    /// a static string) or from the command line.
    pub fn locate(&self, s: &str) -> Option<Location> {
        let (mut id, mut off) = self.find(s)?;
        loop {
            match self.origin(id) {
                Origin::Derived { in_buf, site } => (id, off) = (in_buf, site),
                Origin::CommandLine => return None,
                Origin::File { name, .. } => {
                    let bufs = self.buffers.borrow();
                    let b = &bufs[id.0 as usize];
                    let starts = b.line_starts.get_or_init(|| line_starts(&b.text));
                    let line0 = starts.partition_point(|&st| st <= off) - 1;
                    let col = b.text[starts[line0]..off].chars().count() + 1;
                    let phys = line0 + 1;
                    let (name, line) = match b.remaps.iter().rev().find(|r| r.phys <= phys) {
                        Some(r) => (r.name.clone(), r.logical + (phys - r.phys)),
                        None => (name, phys),
                    };
                    return Some(Location {
                        file: id,
                        name,
                        line,
                        col,
                    });
                }
            }
        }
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let b = text.as_bytes();
    let mut v = vec![0];
    for i in 0..b.len() {
        if b[i] == b'\n' || (b[i] == b'\r' && b.get(i + 1) != Some(&b'\n')) {
            v.push(i + 1);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_map_follows_derived_text() {
        let sm = SourceMap::new();
        let (f, text) = sm.add(
            "a\n`M\n".into(),
            Origin::File {
                name: "x.v".into(),
                parent: None,
            },
        );
        let site = &text[2..4];
        let exp = sm.derive("one two".into(), site);
        let loc = sm.locate(&exp[4..]).unwrap();
        assert_eq!(
            (loc.file, loc.name.as_str(), loc.line, loc.col),
            (f, "x.v", 2, 1)
        );
        assert!(sm.locate(" ").is_none());

        sm.remap_lines(f, 2, 100, "y.v".into());
        let loc = sm.locate(site).unwrap();
        assert_eq!((loc.name.as_str(), loc.line), ("y.v", 100));
    }

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
