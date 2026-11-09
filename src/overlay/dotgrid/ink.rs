//! How a dot frame's ink is spread: FO-1's measure of whether a dense view
//! reads as a density gradient or as solid blocks. A frame of white dots on
//! black is cut into square blocks; each inked block's mean ink says how
//! dense it looks, "saturated" blocks are the solid squares the user
//! reported, and the number of occupied deciles is how many distinct
//! densities the eye can read.

/// [`ink_report`]'s summary of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InkReport {
    /// Blocks with any meaningful ink (mean > 0.02).
    pub blocks: u32,
    /// Percent of those blocks whose mean ink is at least 0.95.
    pub saturated_pct: f64,
    /// Distinct deciles (`floor(mean * 10)`, 0..=9) the counted blocks fall in.
    pub deciles_occupied: u32,
    /// Sum of every pixel's ink, in pixels of full ink.
    pub total_ink: f64,
}

/// Mean ink below which a block counts as empty.
const INKED: f64 = 0.02;
/// Mean ink at or above which a block counts as saturated.
const SATURATED: f64 = 0.95;

/// `pixels`: RGBA8 of white dots on black; ink = R / 255. Blocks of `block`×`block`
/// px whose mean ink > 0.02 are counted; "saturated" = mean ≥ 0.95.
/// A partial block at the right or bottom edge is averaged over the pixels
/// it has.
#[must_use]
pub fn ink_report(pixels: &[u8], width: u32, height: u32, block: u32) -> InkReport {
    assert!(block > 0, "a block needs at least one pixel");
    assert!(pixels.len() >= (width as usize) * (height as usize) * 4, "RGBA8 of width x height");
    let ink = |x: u32, y: u32| f64::from(pixels[((y as usize) * (width as usize) + x as usize) * 4]) / 255.0;
    let (mut blocks, mut saturated, mut deciles, mut total_ink) = (0u32, 0u32, [false; 10], 0.0f64);
    for by in (0..height).step_by(block as usize) {
        for bx in (0..width).step_by(block as usize) {
            let (x1, y1) = ((bx + block).min(width), (by + block).min(height));
            let mut sum = 0.0;
            for y in by..y1 {
                for x in bx..x1 {
                    sum += ink(x, y);
                }
            }
            total_ink += sum;
            let mean = sum / f64::from((x1 - bx) * (y1 - by));
            if mean <= INKED {
                continue;
            }
            blocks += 1;
            saturated += u32::from(mean >= SATURATED);
            deciles[((mean * 10.0).floor() as usize).min(9)] = true;
        }
    }
    InkReport {
        blocks,
        saturated_pct: if blocks == 0 { 0.0 } else { 100.0 * f64::from(saturated) / f64::from(blocks) },
        deciles_occupied: deciles.iter().filter(|&&d| d).count() as u32,
        total_ink,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solid_white_frame_is_all_saturated_and_one_decile() {
        let px = vec![255u8; 64 * 64 * 4];
        let r = ink_report(&px, 64, 64, 32);
        assert_eq!((r.blocks, r.saturated_pct, r.deciles_occupied), (4, 100.0, 1));
    }

    #[test]
    fn a_ramp_occupies_every_decile_and_none_saturates_below_095() {
        // 10 blocks of 32x32 whose value is (k + 0.5) / 10 * 255, k = 0..9
        // (k = 9 is 0.95: counts saturated). Rounded UP to a level: 242.25
        // rounds to 242, 0.949, which would put k = 9 under 0.95; the ceil
        // stays inside decile k for every k.
        let (w, h) = (320u32, 32u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let k = x / 32;
                let v = ((k as f64 + 0.5) / 10.0 * 255.0).ceil() as u8;
                px[((y * w + x) * 4) as usize] = v;
            }
        }
        let r = ink_report(&px, w, h, 32);
        assert_eq!(r.blocks, 10);
        assert_eq!(r.deciles_occupied, 10);
        assert_eq!(r.saturated_pct, 10.0, "only the k = 9 block (0.95) is saturated");
    }

    #[test]
    fn black_blocks_are_not_counted() {
        let px = vec![0u8; 64 * 64 * 4];
        let r = ink_report(&px, 64, 64, 32);
        assert_eq!((r.blocks, r.saturated_pct, r.deciles_occupied, r.total_ink), (0, 0.0, 0, 0.0));
    }
}
