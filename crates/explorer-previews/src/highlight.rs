//! Syntax highlighting for the code preview, via syntect.
//!
//! Runs on a preview worker (never the UI thread). The syntax and
//! theme assets load once, lazily. Highlighting resolves a syntax from
//! the file's extension or first line; files with no real syntax
//! (plain `.txt`, `.log`, prose) return `None` so the caller falls
//! back to an un-highlighted [`crate::Preview::Text`].

use std::path::Path;
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::CodeSpan;

/// Cap on highlighted lines — bounds both the highlighting cost and the
/// span count the UI lays out. Matches the UI's text line cap.
pub(crate) const MAX_CODE_LINES: usize = 400;

struct Assets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        // The app is dark-only; a neutral dark theme reads well on the
        // sunken code surface. Fall back to any bundled theme.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .expect("syntect ships default themes");
        Assets { syntaxes, theme }
    })
}

/// Highlight `text` for the file at `path`: one span list per line, up
/// to `MAX_CODE_LINES`. Returns `None` when no syntax matches (the
/// caller then shows plain text rather than uniformly-colored prose).
pub(crate) fn highlight(path: &Path, text: &str) -> Option<Vec<Vec<CodeSpan>>> {
    let assets = assets();
    let syntax = syntax_for(path, text, &assets.syntaxes)?;
    let mut highlighter = HighlightLines::new(syntax, &assets.theme);
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(text).take(MAX_CODE_LINES) {
        // A highlight failure mid-file shouldn't sink the whole preview;
        // stop and keep what we have.
        let Ok(ranges) = highlighter.highlight_line(line, &assets.syntaxes) else {
            break;
        };
        lines.push(to_spans(&ranges));
    }
    (!lines.is_empty()).then_some(lines)
}

/// Resolve a syntax by extension, then by first line (shebangs,
/// modelines). "Plain Text" counts as no syntax — prose gets no
/// highlighting.
fn syntax_for<'a>(path: &Path, text: &str, syntaxes: &'a SyntaxSet) -> Option<&'a SyntaxReference> {
    let plain = syntaxes.find_syntax_plain_text();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(syntax) = syntaxes.find_syntax_by_extension(ext) {
            if !std::ptr::eq(syntax, plain) {
                return Some(syntax);
            }
        }
    }
    if let Some(first) = text.lines().next() {
        if let Some(syntax) = syntaxes.find_syntax_by_first_line(first) {
            return Some(syntax);
        }
    }
    None
}

/// Convert a highlighted line to spans, merging adjacent same-color
/// runs (syntect emits many tiny runs) and dropping the line ending.
fn to_spans(ranges: &[(syntect::highlighting::Style, &str)]) -> Vec<CodeSpan> {
    let mut spans: Vec<CodeSpan> = Vec::new();
    for (style, piece) in ranges {
        let piece = piece.trim_end_matches(['\r', '\n']);
        if piece.is_empty() {
            continue;
        }
        let fg = style.foreground;
        let color = [fg.r, fg.g, fg.b];
        match spans.last_mut() {
            Some(last) if last.color == color => last.text.push_str(piece),
            _ => spans.push(CodeSpan {
                text: piece.to_string(),
                color,
            }),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn known_language_highlights_into_colored_spans() {
        let src = "fn main() {\n    let x = 42;\n}\n";
        let lines = highlight(&PathBuf::from("a.rs"), src).expect("rust highlights");
        assert_eq!(lines.len(), 3);
        // The keyword and the number should not share the default text
        // color — i.e. more than one distinct color appears.
        let colors: std::collections::HashSet<_> =
            lines.iter().flatten().map(|s| s.color).collect();
        assert!(colors.len() > 1, "expected multiple token colors");
        // Round-trips the source text.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains("fn main()"));
        assert!(joined.contains("42"));
    }

    #[test]
    fn plain_text_is_not_highlighted() {
        // No syntax for a `.txt` / unknown extension → fall back to text.
        assert!(highlight(&PathBuf::from("notes.txt"), "just prose here\n").is_none());
    }

    #[test]
    fn shebang_resolves_a_syntax() {
        let src = "#!/bin/bash\necho hi\n";
        // No useful extension, but the shebang first line identifies it.
        assert!(highlight(&PathBuf::from("runme"), src).is_some());
    }
}
