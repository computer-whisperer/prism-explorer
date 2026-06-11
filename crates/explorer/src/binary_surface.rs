use damascene_core::surface::AppTexture;
use damascene_wgpu::StreamingTexture;
use explorer_previews::BinaryPreview;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BinarySurfaceMetrics {
    pub shown: usize,
    pub cols: u32,
    pub rows: u32,
}

pub(crate) struct BinarySurface {
    texture: StreamingTexture,
    metrics: BinarySurfaceMetrics,
}

impl BinarySurface {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        Self {
            texture: StreamingTexture::new(device, wgpu::TextureFormat::Rgba8UnormSrgb, 1, 1),
            metrics: BinarySurfaceMetrics::default(),
        }
    }

    pub(crate) fn app_texture(&self) -> AppTexture {
        self.texture.app_texture()
    }

    pub(crate) fn metrics(&self) -> BinarySurfaceMetrics {
        self.metrics
    }

    pub(crate) fn write(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        preview: &BinaryPreview,
        logical_w: f32,
        logical_h: f32,
        scale_factor: f32,
    ) {
        let width = ((logical_w * scale_factor).ceil() as u32).clamp(96, 1536);
        let height = ((logical_h * scale_factor).ceil() as u32).clamp(96, 1536);
        let (pixels, metrics) = rasterize_byte_map(&preview.bytes, width, height);
        self.texture
            .write_frame_if_changed(device, queue, width, height, &pixels);
        self.metrics = metrics;
    }
}

fn rasterize_byte_map(bytes: &[u8], width: u32, height: u32) -> (Vec<u8>, BinarySurfaceMetrics) {
    let width = width.max(1);
    let height = height.max(1);
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&[18, 21, 25, 255]);
    }
    if bytes.is_empty() {
        return (
            pixels,
            BinarySurfaceMetrics {
                shown: 0,
                cols: 0,
                rows: 0,
            },
        );
    }

    let (cols, rows) = grid_for(bytes.len(), width, height);
    let shown = bytes
        .len()
        .min((cols as usize).saturating_mul(rows as usize));
    for (i, &byte) in bytes.iter().take(shown).enumerate() {
        let col = i as u32 % cols;
        let row = i as u32 / cols;
        let x0 = col * width / cols;
        let x1 = ((col + 1) * width / cols).max(x0 + 1).min(width);
        let y0 = row * height / rows;
        let y1 = ((row + 1) * height / rows).max(y0 + 1).min(height);
        let color = byte_color(byte);
        for y in y0..y1 {
            let row_start = y as usize * width as usize * 4;
            for x in x0..x1 {
                let idx = row_start + x as usize * 4;
                pixels[idx..idx + 4].copy_from_slice(&color);
            }
        }
    }

    (pixels, BinarySurfaceMetrics { shown, cols, rows })
}

fn grid_for(len: usize, width: u32, height: u32) -> (u32, u32) {
    if len == 0 {
        return (0, 0);
    }
    let capacity = width as usize * height as usize;
    if len >= capacity {
        return (width, height);
    }

    let aspect = width as f32 / height.max(1) as f32;
    let rows = ((len as f32 / aspect).sqrt().ceil() as u32).clamp(1, height);
    let cols = (len as u32).div_ceil(rows).clamp(1, width);
    let rows = (len as u32).div_ceil(cols).clamp(1, height);
    if (cols as usize).saturating_mul(rows as usize) < len {
        return (width, height);
    }
    (cols, rows)
}

fn byte_color(byte: u8) -> [u8; 4] {
    let v = byte as u16;
    match byte {
        0x00 => [12, 15, 19, 255],
        0xff => [238, 242, 247, 255],
        b'\t' | b'\n' | b'\r' | b' ' => [42, 128, (150 + v / 5) as u8, 255],
        0x01..=0x1f | 0x7f => [(122 + v / 3) as u8, 49, 62, 255],
        0x21..=0x7e => [44, (108 + v / 4) as u8, 78, 255],
        _ => [(126 + v / 4) as u8, (86 + v / 8) as u8, 38, 255],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_fills_available_shape_for_small_samples() {
        let (_pixels, metrics) = rasterize_byte_map(&[0; 100], 80, 40);
        assert_eq!(metrics.shown, 100);
        assert!(metrics.cols > metrics.rows);
        assert!(metrics.cols <= 80);
        assert!(metrics.rows <= 40);
    }

    #[test]
    fn one_byte_uses_one_cell() {
        let (_pixels, metrics) = rasterize_byte_map(&[0], 80, 40);
        assert_eq!(metrics.shown, 1);
        assert_eq!(metrics.cols, 1);
        assert_eq!(metrics.rows, 1);
    }

    #[test]
    fn grid_caps_to_texture_capacity() {
        let (_pixels, metrics) = rasterize_byte_map(&[0xff; 200], 10, 10);
        assert_eq!(metrics.shown, 100);
        assert_eq!(metrics.cols, 10);
        assert_eq!(metrics.rows, 10);
    }
}
