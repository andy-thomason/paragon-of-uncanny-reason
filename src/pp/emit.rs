//! Write preprocessor output as text, as `-E` does.

use super::Token;
use crate::source::{BufId, Origin, SourceMap};

#[derive(Clone, Copy, Debug, Default)]
pub struct EmitOptions {
    /// Emit `` `line `` markers (plain `-E`). Without them (`-E -P`), lines
    /// that are empty or only whitespace are dropped.
    pub line_markers: bool,
}

pub fn write(sm: &SourceMap, toks: &[Token<'_>], opts: EmitOptions) -> String {
    if opts.line_markers {
        write_with_markers(sm, toks)
    } else {
        write_plain(toks)
    }
}

fn write_plain(toks: &[Token<'_>]) -> String {
    let text: String = toks.iter().map(|t| t.text()).collect();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if !line.trim().is_empty() {
            out.push_str(line.trim_end_matches(['\n', '\r']));
            out.push('\n');
        }
    }
    out
}

/// Emit a `` `line `` marker at the start of an output line whenever the
/// line's first token does not come from the line the reader would expect.
fn write_with_markers(sm: &SourceMap, toks: &[Token<'_>]) -> String {
    let parent = |id: BufId| match sm.origin(id) {
        Origin::File { parent, .. } => parent,
        _ => None,
    };
    let mut out = String::new();
    let mut cur: Option<(BufId, String, usize)> = None;
    let mut line_start = true;
    for t in toks {
        let text = t.text();
        if line_start {
            if let Some(loc) = sm.locate(text) {
                if cur.is_none() {
                    // Output that starts inside an include still begins with
                    // the top-level file.
                    let mut root = loc.file;
                    while let Some(p) = parent(root) {
                        root = p;
                    }
                    if root != loc.file
                        && let Origin::File { name, .. } = sm.origin(root)
                    {
                        out.push_str(&format!("`line 1 \"{name}\" 1\n"));
                    }
                }
                let level = match &cur {
                    None => Some(1),
                    Some((f, name, line)) if *f == loc.file => {
                        (*name != loc.name || *line != loc.line).then_some(0)
                    }
                    Some((f, ..)) if parent(loc.file) == Some(*f) => Some(1),
                    Some((f, ..)) if parent(*f) == Some(loc.file) => Some(2),
                    Some(_) => Some(0),
                };
                if let Some(level) = level {
                    out.push_str(&format!("`line {} \"{}\" {level}\n", loc.line, loc.name));
                }
                cur = Some((loc.file, loc.name, loc.line));
            }
            line_start = false;
        }
        out.push_str(text);
        let newlines = text.matches('\n').count();
        if newlines > 0 {
            if let Some((_, _, line)) = &mut cur {
                *line += newlines;
            }
            line_start = text.ends_with('\n');
        }
    }
    out
}
