//! Bounded live updates for client-owned top-level chrome.
//!
//! A focus transition changes only the shared [`WindowFrame`] titlebar and
//! border. Repainting through the ordinary alternating-buffer path can expose
//! stale application content if only those regions are updated, while a full
//! repaint transports an entire window. `WindowFramePatch` instead snapshots
//! the frame's immutable paint state, rasterizes only the next bounded tile,
//! and applies it to the already-attached buffer via
//! `pmd_surface.patch_current`, one request per event-loop turn. The alternate
//! buffer is never attached or modified.

use crate::draw::{BufferPool, CurrentPatch, Rect, BYTES_PER_PIXEL};
use crate::protocol::{ClientError, Connection};
use crate::widget::WindowFrame;
use crate::Window;

/// Result of advancing a bounded chrome patch by at most one wire request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowFramePatchProgress {
    /// A prior outbound suffix or staged full commit must drain first.
    Deferred,
    /// One patch was queued and more frame regions remain.
    Pending,
    /// Every frame region has been queued, or the frame had no pixels.
    Complete,
    /// The pool has no attached current buffer; the caller must perform its
    /// normal full-frame initialization/repaint instead.
    Unavailable,
}

/// A snapshot of one target `WindowFrame` visual state, split into requests
/// that each obey the display protocol's 24 KiB inline-pixel ceiling.
pub struct WindowFramePatch {
    frame: WindowFrame,
    tiles: Vec<Rect>,
    next: usize,
}

impl WindowFramePatch {
    pub fn new(frame: &WindowFrame) -> Self {
        let mut tiles = Vec::new();
        for region in frame.focus_damage_regions() {
            for damage in tile_region(region, display_proto::MAX_SURFACE_PATCH_BYTES) {
                tiles.push(damage);
            }
        }
        Self {
            frame: frame.paint_snapshot(),
            tiles,
            next: 0,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.next == self.tiles.len()
    }

    /// Replace any partially-applied snapshot with the newest visual state.
    /// Already-sent tiles remain harmless: the replacement restarts at the
    /// leading titlebar tile and eventually overwrites every focus-sensitive
    /// frame pixel with the latest state.
    pub fn replace(&mut self, frame: &WindowFrame) {
        *self = Self::new(frame);
    }

    pub fn remaining_tiles(&self) -> usize {
        self.tiles.len().saturating_sub(self.next)
    }

    pub fn completed_tiles(&self) -> usize {
        self.next
    }

    pub fn total_pixel_bytes(&self) -> usize {
        self.tiles
            .iter()
            .map(|tile| tile.width as usize * tile.height as usize * BYTES_PER_PIXEL)
            .sum()
    }

    /// Queue at most one `patch_current` request. A successful call advances
    /// exactly one tile; deferred/unavailable calls preserve the cursor.
    pub fn progress<C: Connection>(
        &mut self,
        pool: &mut BufferPool,
        window: &mut Window<'_, C>,
    ) -> Result<WindowFramePatchProgress, ClientError> {
        let Some(damage) = self.tiles.get(self.next).copied() else {
            return Ok(WindowFramePatchProgress::Complete);
        };
        // Avoid even the current tile's temporary pixels while another
        // transport suffix or full commit must drain first.
        if pool.commit_pending() || window.outbound_pending() {
            return Ok(WindowFramePatchProgress::Deferred);
        }
        if pool.current_index().is_none() {
            return Ok(WindowFramePatchProgress::Unavailable);
        }
        let pixels = self
            .frame
            .rasterize_region(damage)
            .expect("frame damage tiles remain inside the paint snapshot");
        debug_assert!(pixels.len() <= display_proto::MAX_SURFACE_PATCH_BYTES);
        match pool.patch_current(window, damage, &pixels)? {
            CurrentPatch::Deferred { .. } => Ok(WindowFramePatchProgress::Deferred),
            CurrentPatch::Unavailable => Ok(WindowFramePatchProgress::Unavailable),
            CurrentPatch::Patched { .. } => {
                self.next += 1;
                Ok(if self.is_complete() {
                    WindowFramePatchProgress::Complete
                } else {
                    WindowFramePatchProgress::Pending
                })
            }
        }
    }
}

fn tile_region(region: Rect, max_bytes: usize) -> Vec<Rect> {
    if region.is_empty() || max_bytes < BYTES_PER_PIXEL {
        return Vec::new();
    }
    let max_pixels = max_bytes / BYTES_PER_PIXEL;
    let tile_width = (region.width as usize).min(max_pixels).max(1);
    let tile_height = (max_pixels / tile_width).max(1);
    let mut tiles = Vec::new();
    let mut y = 0usize;
    while y < region.height as usize {
        let height = tile_height.min(region.height as usize - y);
        let mut x = 0usize;
        while x < region.width as usize {
            let width = tile_width.min(region.width as usize - x);
            tiles.push(Rect::new(
                region.x + x as i32,
                region.y + y as i32,
                width as u32,
                height as u32,
            ));
            x += width;
        }
        y += height;
    }
    tiles
}
