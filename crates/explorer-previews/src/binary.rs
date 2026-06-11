//! Binary previews for files that are not recognized as text or a
//! richer media/document format.

use crate::Preview;

#[derive(Debug)]
pub struct BinaryPreview {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub entropy_bits: f32,
    pub printable_fraction: f32,
    pub nul_fraction: f32,
    pub ff_fraction: f32,
    pub distinct_values: usize,
}

pub(crate) fn preview_binary(bytes: Vec<u8>, truncated: bool) -> Preview {
    Preview::Binary(BinaryPreview::from_bytes(bytes, truncated))
}

impl BinaryPreview {
    pub fn from_bytes(bytes: Vec<u8>, truncated: bool) -> Self {
        let mut counts = [0usize; 256];
        let mut printable = 0usize;
        let mut nul = 0usize;
        let mut ff = 0usize;
        for &b in &bytes {
            counts[b as usize] += 1;
            if b == 0 {
                nul += 1;
            }
            if b == 0xff {
                ff += 1;
            }
            if b.is_ascii_graphic() || b == b' ' {
                printable += 1;
            }
        }

        let len = bytes.len().max(1) as f32;
        let entropy_bits = counts
            .iter()
            .filter(|&&count| count > 0)
            .map(|&count| {
                let p = count as f32 / len;
                -p * p.log2()
            })
            .sum();
        let distinct_values = counts.iter().filter(|&&count| count > 0).count();

        Self {
            bytes,
            truncated,
            entropy_bits,
            printable_fraction: printable as f32 / len,
            nul_fraction: nul as f32 / len,
            ff_fraction: ff as f32 / len,
            distinct_values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_basic_binary_stats() {
        let preview = BinaryPreview::from_bytes(vec![0, 0, 0xff, b'A'], false);
        assert_eq!(preview.bytes.len(), 4);
        assert!(!preview.truncated);
        assert_eq!(preview.distinct_values, 3);
        assert_eq!(preview.nul_fraction, 0.5);
        assert_eq!(preview.ff_fraction, 0.25);
        assert_eq!(preview.printable_fraction, 0.25);
        assert!(preview.entropy_bits > 0.0);
    }
}
