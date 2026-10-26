//! RGB parade and luma waveform of the actual displayed SDR picture.
pub fn waveform(image: &crate::playback::ImageBuf, parade: bool) -> crate::playback::ImageBuf {
    let (w, h) = (384usize, 160usize);
    let mut density = vec![[0u32; 3]; w * h];
    let width = image.width as usize;
    for (y, row) in image
        .rgb
        .chunks_exact(width.max(1) * 3)
        .enumerate()
        .step_by((image.height as usize / 256).max(1))
    {
        let _ = y;
        for (x, p) in row
            .chunks_exact(3)
            .enumerate()
            .step_by((width / 512).max(1))
        {
            if parade {
                for c in 0..3 {
                    let px = c * w / 3 + x * w / 3 / width.max(1);
                    let py = (h - 1) * (255 - p[c] as usize) / 255;
                    density[py * w + px][c] += 1;
                }
            } else {
                let luma = (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
                    .round() as usize;
                let px = x * w / width.max(1);
                let py = (h - 1) * (255 - luma.min(255)) / 255;
                for c in 0..3 {
                    density[py * w + px][c] += 1;
                }
            }
        }
    }
    let mut output = crate::playback::ImageBuf::black(w as u32, h as u32);
    for (y, row) in output.rgb.chunks_exact_mut(w * 3).enumerate() {
        for (x, p) in row.chunks_exact_mut(3).enumerate() {
            for c in 0..3 {
                let grid = if y % (h / 4) == 0 { 32 } else { 12 };
                p[c] = (grid as f32 + (density[y * w + x][c] as f32).sqrt() * 36.).min(255.) as u8;
            }
        }
    }
    output
}
