//! One frame as the map drew it, read back from its own surface.
//!
//! [`crate::map_view::MapView::grab_next_frame`] asks for the next frame;
//! `paint` copies that frame's colour texture into a buffer just before it
//! submits, and hands the buffer over as a [`FrameGrab`]. The caller reads
//! it on its own thread ([`FrameGrab::read`]), so the UI thread never waits
//! on the GPU.
//!
//! The same opt-in that MapLibre GL JS makes with `preserveDrawingBuffer`
//! (`src/ui/map.ts`, off by default): a surface that can be copied from
//! (`COPY_SRC`) can cost the compositor a little, so a map configures its
//! surface that way only after the first grab is asked for. Everything the
//! map draws is in it -- basemap, overlays, dots, flows -- and nothing the
//! host draws over the map (gpui panels, the logo) is.

/// A frame copied off the surface, not yet read.
pub struct FrameGrab {
    device: wgpu::Device,
    buffer: crate::render::alloc::TrackedBuffer,
    width: u32,
    height: u32,
    bytes_per_row: u32,
    bgra: bool,
}

/// A frame as straight RGBA8, row-major, opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabbedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Whether frames in `format` can be grabbed, and if so whether its bytes
/// are BGRA (swapped on read).
#[must_use]
pub fn grab_layout(format: wgpu::TextureFormat) -> Option<bool> {
    use wgpu::TextureFormat as F;
    match format {
        F::Bgra8Unorm | F::Bgra8UnormSrgb => Some(true),
        F::Rgba8Unorm | F::Rgba8UnormSrgb => Some(false),
        _ => None,
    }
}

/// Records the copy of `texture` into `encoder`. `None` when the texture
/// cannot be copied from or its format is not 8-bit RGBA/BGRA.
#[must_use]
pub fn encode(ctx: &crate::render::GpuContext, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture) -> Option<FrameGrab> {
    if !texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        return None;
    }
    let bgra = grab_layout(texture.format())?;
    let (width, height) = (texture.width(), texture.height());
    let bytes_per_row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = crate::render::alloc::TrackedBuffer::new(
        &ctx.device,
        &wgpu::BufferDescriptor {
            label: Some("frame-grab"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        },
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(height) },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    Some(FrameGrab { device: ctx.device.clone(), buffer, width, height, bytes_per_row, bgra })
}

impl FrameGrab {
    /// The frame's pixels. Blocks until the GPU has finished the copy, so
    /// call it off the UI thread.
    ///
    /// # Errors
    /// When the buffer cannot be mapped.
    pub fn read(self) -> Result<GrabbedFrame, String> {
        let (width, height) = (self.width, self.height);
        let swap = self.bgra;
        let rgba = self.read_bytes(swap)?;
        Ok(GrabbedFrame { width, height, rgba })
    }

    /// The frame's pixels as BGRA8, opaque -- the order gpui's
    /// `RenderImage` holds them in, so a frame shown through gpui needs no
    /// second pass over its bytes. Blocks like [`Self::read`].
    ///
    /// # Errors
    /// When the buffer cannot be mapped.
    pub fn read_bgra(self) -> Result<(u32, u32, Vec<u8>), String> {
        let (width, height) = (self.width, self.height);
        let swap = !self.bgra;
        Ok((width, height, self.read_bytes(swap)?))
    }

    /// Maps the copy and unpads it, swapping R and B when `swap`.
    fn read_bytes(self, swap: bool) -> Result<Vec<u8>, String> {
        let slice = self.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("the GPU did not finish the copy: {e}"))?;
        rx.recv().map_err(|_| "the copy was dropped".to_string())?.map_err(|e| format!("the copy could not be read: {e}"))?;
        let data = slice.get_mapped_range().map_err(|e| format!("the copy could not be read: {e}"))?;
        let bytes = unpad(&data, self.width, self.height, self.bytes_per_row, swap);
        drop(data);
        self.buffer.unmap();
        Ok(bytes)
    }
}

/// Drops each row's padding, swaps BGRA to RGBA and makes every pixel
/// opaque (the surface is premultiplied; a picture has no backdrop).
#[must_use]
pub fn unpad(data: &[u8], width: u32, height: u32, bytes_per_row: u32, bgra: bool) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(row * height as usize);
    for line in data.chunks(bytes_per_row as usize).take(height as usize) {
        for px in line[..row].as_chunks::<4>().0 {
            let (r, b) = if bgra { (px[2], px[0]) } else { (px[0], px[2]) };
            out.extend_from_slice(&[r, px[1], b, 255]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpad_drops_row_padding_swaps_bgra_and_makes_pixels_opaque() {
        // Two pixels a row, padded to 12 bytes, two rows.
        let data = [1, 2, 3, 0, 4, 5, 6, 7, 9, 9, 9, 9, 10, 20, 30, 40, 50, 60, 70, 80, 9, 9, 9, 9];
        assert_eq!(unpad(&data, 2, 2, 12, true), [3, 2, 1, 255, 6, 5, 4, 255, 30, 20, 10, 255, 70, 60, 50, 255]);
        assert_eq!(unpad(&data, 2, 2, 12, false), [1, 2, 3, 255, 4, 5, 6, 255, 10, 20, 30, 255, 50, 60, 70, 255]);
    }

    #[test]
    fn only_eight_bit_colour_formats_are_grabbed() {
        assert_eq!(grab_layout(wgpu::TextureFormat::Bgra8UnormSrgb), Some(true));
        assert_eq!(grab_layout(wgpu::TextureFormat::Rgba8Unorm), Some(false));
        assert_eq!(grab_layout(wgpu::TextureFormat::Rgba16Float), None);
    }
}
