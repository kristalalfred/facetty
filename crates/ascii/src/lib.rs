//! Turns video frames into grids of colored glyphs.
//!
//! The analysis is a CPU port of the passes in Acerola's ASCII shader
//! (AcerolaFX_ASCII.fx): a thresholded difference of Gaussians finds edges,
//! a Sobel filter gives their orientation, and each cell either draws the
//! dominant edge direction or a fill glyph picked by luminance. The shader
//! rasterizes glyph bitmaps into 8x8 pixel tiles; here a tile is a terminal
//! cell and the output is the glyph itself.

mod analysis;
mod codec;
pub mod reel;

pub use analysis::{Analyzer, Params};
pub use codec::{DecodeError, Decoder, Encoder, merge};

pub const FILL_GLYPHS: usize = 10;

/// Indices 0..10 are fill glyphs ordered by density; 10..14 are edges.
pub const GLYPHS: [char; 14] = [
    ' ', '.', ';', 'c', 'o', 'P', 'O', '?', '@', '█', '|', '-', '/', '\\',
];

pub const EDGE_VERTICAL: u8 = 10;
pub const EDGE_HORIZONTAL: u8 = 11;
pub const EDGE_RISING: u8 = 12;
pub const EDGE_FALLING: u8 = 13;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Packed RGB, row-major, no padding.
    pub rgb: Vec<u8>,
}

impl Image {
    pub fn new(width: usize, height: usize, rgb: Vec<u8>) -> Self {
        assert_eq!(rgb.len(), width * height * 3, "RGB buffer size mismatch");
        Self { width, height, rgb }
    }
}

/// Per-pixel scene depth and surface normals for an [`Image`], as a 3D
/// renderer outputs them.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub depth: Vec<f32>,
    pub normal: Vec<[f32; 3]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub glyph: u8,
    pub rgb: [u8; 3],
}

impl Cell {
    pub fn char(self) -> char {
        GLYPHS.get(self.glyph as usize).copied().unwrap_or(' ')
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
}

impl Frame {
    pub fn blank(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
        }
    }

    pub fn get(&self, col: u16, row: u16) -> Cell {
        self.cells[row as usize * self.cols as usize + col as usize]
    }

    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.cells
            .chunks(self.cols as usize)
            .map(|row| row.iter().map(|c| c.char()).collect())
    }
}
