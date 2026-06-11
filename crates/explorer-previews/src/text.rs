//! Text previews: known text/code types, plus a sniffing fallback for
//! everything no other handler claimed.

use std::io::Read;
use std::path::Path;

use crate::{binary::preview_binary, extension, Preview, PreviewHandler};

/// How much of the file a text preview reads. Bounded: this often runs
/// against multi-gigabyte logs on a slow mount.
const PREFIX_BYTES: u64 = 128 * 1024;

/// NUL within this much of the head marks the file as binary (the
/// classic `file`/git heuristic).
const SNIFF_BYTES: usize = 8 * 1024;

/// Extensions that are text by convention. Routing only — content
/// still goes through the binary sniff in case of a lying name.
const TEXT_EXTENSIONS: &[&str] = &[
    // prose & docs
    "txt",
    "md",
    "markdown",
    "rst",
    "org",
    "adoc",
    "tex",
    "bib",
    "csv",
    "tsv",
    "log",
    // config
    "toml",
    "yaml",
    "yml",
    "json",
    "jsonc",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "kdl",
    "ron",
    "xml",
    "plist",
    "desktop",
    "service",
    "rules",
    // code
    "rs",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "hh",
    "cxx",
    "py",
    "js",
    "mjs",
    "ts",
    "tsx",
    "jsx",
    "go",
    "java",
    "kt",
    "swift",
    "rb",
    "pl",
    "lua",
    "zig",
    "nim",
    "hs",
    "ml",
    "el",
    "lisp",
    "clj",
    "scala",
    "cs",
    "fs",
    "sql",
    "r",
    "jl",
    "dart",
    "php",
    // shell & build
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmake",
    "mk",
    "ninja",
    "dockerfile",
    "nix",
    "gradle",
    "bazel",
    "bzl",
    // web
    "html",
    "htm",
    "css",
    "scss",
    "less",
    "svg",
    "vue",
    "svelte",
    // hardware & embedded (this household has firmware)
    "v",
    "sv",
    "vhd",
    "vhdl",
    "dts",
    "dtsi",
    "ld",
    "s",
    "asm",
    "hex",
    "gcode",
    // misc
    "patch",
    "diff",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "lock",
];

/// File names that are text without an extension.
const TEXT_NAMES: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "notice",
    "authors",
    "contributors",
    "changelog",
    "news",
    "todo",
    "makefile",
    "gnumakefile",
    "dockerfile",
    "justfile",
    "rakefile",
    "gemfile",
    "vagrantfile",
    "cmakelists.txt",
    "kconfig",
    "pkgbuild",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".editorconfig",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".vimrc",
];

fn read_prefix(path: &Path) -> anyhow::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let read = file.take(PREFIX_BYTES + 1).read_to_end(&mut buf)?;
    let truncated = read as u64 > PREFIX_BYTES;
    buf.truncate(PREFIX_BYTES as usize);
    Ok((buf, truncated))
}

fn looks_binary(prefix: &[u8]) -> bool {
    prefix[..prefix.len().min(SNIFF_BYTES)].contains(&0)
}

fn to_preview(prefix: Vec<u8>, truncated: bool) -> Preview {
    if looks_binary(&prefix) {
        return preview_binary(prefix, truncated);
    }
    Preview::Text {
        text: String::from_utf8_lossy(&prefix).into_owned(),
        truncated,
    }
}

/// Known text/code types, by extension or canonical file name.
pub struct TextHandler;

impl PreviewHandler for TextHandler {
    fn name(&self) -> &'static str {
        "text"
    }

    fn claims(&self, path: &Path) -> bool {
        if extension(path).is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str())) {
            return true;
        }
        path.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .is_some_and(|n| TEXT_NAMES.contains(&n.as_str()))
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        let (prefix, truncated) = read_prefix(path)?;
        Ok(to_preview(prefix, truncated))
    }
}

/// Last in the chain: claims everything, reads one bounded prefix, and
/// calls it text or binary. This is where unknown extensions and
/// extensionless files get their one `open`.
pub struct TextFallbackHandler;

impl PreviewHandler for TextFallbackHandler {
    fn name(&self) -> &'static str {
        "text-fallback"
    }

    fn claims(&self, _path: &Path) -> bool {
        true
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        let (prefix, truncated) = read_prefix(path)?;
        if prefix.is_empty() {
            return Ok(Preview::Unsupported {
                reason: "empty file".into(),
            });
        }
        Ok(to_preview(prefix, truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_and_binary_sniff() {
        let dir = std::env::temp_dir().join(format!("explorer-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let big = dir.join("big.log");
        std::fs::write(&big, "x".repeat(PREFIX_BYTES as usize + 100)).unwrap();
        match TextHandler.load(&big).unwrap() {
            Preview::Text { text, truncated } => {
                assert!(truncated);
                assert_eq!(text.len(), PREFIX_BYTES as usize);
            }
            _ => panic!("expected text"),
        }

        // A lying extension: NULs in a ".txt" still read as binary.
        let liar = dir.join("liar.txt");
        std::fs::write(&liar, [b'a', 0, b'b']).unwrap();
        assert!(TextHandler.claims(&liar));
        match TextHandler.load(&liar).unwrap() {
            Preview::Binary(preview) => {
                assert_eq!(preview.bytes, vec![b'a', 0, b'b']);
                assert!(!preview.truncated);
            }
            _ => panic!("expected binary verdict"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
