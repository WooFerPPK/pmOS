//! Deterministic PBM terminal-font atlas generator.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "../../toolkit/src/draw/font.rs"]
#[allow(dead_code)]
mod toolkit_font;

type Result<T> = std::result::Result<T, String>;

const ATLAS_COLUMNS: usize = 16;
const ATLAS_ROWS: usize = 16;
const CELL_WIDTH: usize = 8;

pub fn run(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return Err("gen-font-assets takes no arguments".to_string());
    }
    let root = repo_root()?;
    let font_dir = root.join("crates/term/assets/fonts");
    fs::create_dir_all(&font_dir).map_err(|error| error.to_string())?;

    write_font(
        &font_dir.join("pc-vga-16.pbm"),
        "PMos VGA-style ASCII atlas; codepoint = cell row * 16 + column",
        16,
        1,
    )?;
    write_font(
        &font_dir.join("unifont-mono-14.pbm"),
        "PMos compact Unicode-ready ASCII atlas; codepoint = cell row * 16 + column",
        14,
        0,
    )?;
    println!(
        "[xtask] generated 2 terminal font atlases in {}",
        font_dir.display()
    );
    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().map_err(|error| error.to_string())?;
    loop {
        if dir.join("Cargo.toml").is_file() && dir.join("crates/term").is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("could not find PMos repository root".to_string());
        }
    }
}

fn write_font(path: &Path, description: &str, cell_height: usize, top_pad: usize) -> Result<()> {
    let atlas_width = ATLAS_COLUMNS * CELL_WIDTH;
    let atlas_height = ATLAS_ROWS * cell_height;
    let mut pbm = String::with_capacity(atlas_width * atlas_height + atlas_height + 160);
    writeln!(pbm, "P1").unwrap();
    writeln!(pbm, "# {description}").unwrap();
    writeln!(
        pbm,
        "# cell {CELL_WIDTH}x{cell_height}; 256 codepoint slots"
    )
    .unwrap();
    writeln!(pbm, "{atlas_width} {atlas_height}").unwrap();

    for y in 0..atlas_height {
        let cell_y = y / cell_height;
        let local_y = y % cell_height;
        for x in 0..atlas_width {
            let cell_x = x / CELL_WIDTH;
            let local_x = x % CELL_WIDTH;
            let codepoint = cell_y * ATLAS_COLUMNS + cell_x;
            let set = atlas_pixel(codepoint, local_x, local_y, top_pad);
            pbm.push(if set { '1' } else { '0' });
        }
        pbm.push('\n');
    }
    fs::write(path, pbm).map_err(|error| format!("{}: {error}", path.display()))
}

fn atlas_pixel(codepoint: usize, x: usize, y: usize, top_pad: usize) -> bool {
    if !(toolkit_font::FIRST_CHAR as usize..=toolkit_font::LAST_CHAR as usize).contains(&codepoint)
    {
        return false;
    }
    if x == 0 || x > toolkit_font::GLYPH_WIDTH as usize {
        return false;
    }
    let Some(source_y) = y.checked_sub(top_pad).map(|row| row / 2) else {
        return false;
    };
    if source_y >= toolkit_font::GLYPH_HEIGHT as usize {
        return false;
    }
    toolkit_font::glyph_pixel(
        toolkit_font::glyph_for(char::from_u32(codepoint as u32).unwrap()),
        (x - 1) as u32,
        source_y as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_a_has_lit_pixels_and_control_slots_are_blank() {
        assert!(!atlas_pixel(0, 1, 1, 1));
        assert!((0..16).any(|y| (0..CELL_WIDTH).any(|x| atlas_pixel(b'A' as usize, x, y, 1))));
    }
}
