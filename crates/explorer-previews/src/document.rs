//! Document metadata previews.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::{extension, DetailRow, Preview, PreviewHandler};

const PDF_EXTENSIONS: &[&str] = &["pdf"];
const HEAD_BYTES: usize = 64 * 1024;
const TAIL_BYTES: usize = 512 * 1024;

pub struct PdfHandler;

impl PreviewHandler for PdfHandler {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn claims(&self, path: &Path) -> bool {
        extension(path).is_some_and(|e| PDF_EXTENSIONS.contains(&e.as_str()))
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        let (head, sample) = read_head_tail(path)?;
        if !head.starts_with(b"%PDF-") {
            return Ok(Preview::Unsupported {
                reason: "not a PDF document".into(),
            });
        }

        let version = pdf_version(&head).unwrap_or("unknown");
        let pages = count_pdf_pages(&sample);
        let mut rows = vec![DetailRow {
            label: "Format".into(),
            value: format!("PDF {version}"),
        }];
        rows.push(DetailRow {
            label: "Pages".into(),
            value: if pages == 0 {
                "not found in sampled xref".into()
            } else {
                pages.to_string()
            },
        });
        rows.push(DetailRow {
            label: "Preview".into(),
            value: "metadata only".into(),
        });

        Ok(Preview::Details {
            icon: "file-text",
            title: "PDF document".into(),
            rows,
        })
    }
}

fn read_head_tail(path: &Path) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len <= HEAD_BYTES + TAIL_BYTES {
        let mut data = Vec::with_capacity(len);
        file.read_to_end(&mut data)?;
        return Ok((data.clone(), data));
    }

    let mut head = vec![0; HEAD_BYTES];
    let head_len = file.read(&mut head)?;
    head.truncate(head_len);

    file.seek(SeekFrom::End(-(TAIL_BYTES as i64)))?;
    let mut tail = Vec::with_capacity(TAIL_BYTES);
    file.read_to_end(&mut tail)?;

    let mut sample = Vec::with_capacity(head.len() + tail.len());
    sample.extend_from_slice(&head);
    sample.extend_from_slice(&tail);
    Ok((head, sample))
}

fn pdf_version(head: &[u8]) -> Option<&str> {
    let line = head.split(|&b| b == b'\n' || b == b'\r').next()?;
    let version = line.strip_prefix(b"%PDF-")?;
    std::str::from_utf8(version).ok()
}

fn count_pdf_pages(sample: &[u8]) -> usize {
    let mut count = 0;
    let mut pos = 0;
    while let Some(rel) = find_bytes(&sample[pos..], b"/Type") {
        pos += rel + b"/Type".len();
        while pos < sample.len() && sample[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if sample[pos..].starts_with(b"/Page")
            && !sample
                .get(pos + b"/Page".len())
                .is_some_and(u8::is_ascii_alphabetic)
        {
            count += 1;
        }
    }
    count
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_page_objects_not_pages_nodes() {
        let pdf = br#"%PDF-1.7
1 0 obj << /Type /Pages /Count 2 >> endobj
2 0 obj << /Type /Page /Parent 1 0 R >> endobj
3 0 obj << /Type /Page/Parent 1 0 R >> endobj
"#;
        assert_eq!(pdf_version(pdf), Some("1.7"));
        assert_eq!(count_pdf_pages(pdf), 2);
    }
}
