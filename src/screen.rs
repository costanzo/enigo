use std::error::Error;
use std::fmt::{self, Display as FmtDisplay, Formatter};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Current operating-system authorization for a desktop capability.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionStatus {
    Allowed,
    Denied,
    Unknown,
    Unsupported,
}

/// Opaque identity for a display during the lifetime of a capture backend.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisplayId(pub String);

/// Input-coordinate bounds occupied by one display.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// One display that can be captured and controlled.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub id: DisplayId,
    pub name: Option<String>,
    pub primary: bool,
    pub input_bounds: InputBounds,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Display {
    #[must_use]
    pub fn scale_x(&self) -> f64 {
        f64::from(self.pixel_width) / f64::from(self.input_bounds.width)
    }

    #[must_use]
    pub fn scale_y(&self) -> f64 {
        f64::from(self.pixel_height) / f64::from(self.input_bounds.height)
    }
}

/// A point in screenshot-pixel coordinates.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixelPoint {
    pub x: u32,
    pub y: u32,
}

/// A rectangle in screenshot-pixel coordinates.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixelRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A top-to-bottom, tightly packed RGBA8 capture and its input-coordinate mapping.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureFrame {
    pub display_id: DisplayId,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub input_bounds: InputBounds,
    pub rgba: Vec<u8>,
}

impl CaptureFrame {
    pub fn new(
        display_id: DisplayId,
        pixel_width: u32,
        pixel_height: u32,
        input_bounds: InputBounds,
        rgba: Vec<u8>,
    ) -> CaptureResult<Self> {
        if pixel_width == 0
            || pixel_height == 0
            || input_bounds.width == 0
            || input_bounds.height == 0
        {
            return Err(CaptureError::InvalidFrame(
                "frame dimensions must be positive",
            ));
        }
        let expected = usize::try_from(pixel_width)
            .ok()
            .and_then(|width| {
                usize::try_from(pixel_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(CaptureError::InvalidFrame("frame dimensions are too large"))?;
        if rgba.len() != expected {
            return Err(CaptureError::InvalidFrame(
                "RGBA payload length does not match the frame dimensions",
            ));
        }
        Ok(Self {
            display_id,
            pixel_width,
            pixel_height,
            input_bounds,
            rgba,
        })
    }

    /// Convert a screenshot pixel into the coordinate space used for input injection.
    pub fn input_point(&self, point: PixelPoint) -> CaptureResult<(i32, i32)> {
        if point.x >= self.pixel_width || point.y >= self.pixel_height {
            return Err(CaptureError::InvalidRegion);
        }
        let x = map_axis(
            point.x,
            self.pixel_width,
            self.input_bounds.x,
            self.input_bounds.width,
        )?;
        let y = map_axis(
            point.y,
            self.pixel_height,
            self.input_bounds.y,
            self.input_bounds.height,
        )?;
        Ok((x, y))
    }

    /// Crop a frame while retaining the correct input-coordinate mapping.
    pub fn crop(&self, region: PixelRegion) -> CaptureResult<Self> {
        validate_region(region, self.pixel_width, self.pixel_height)?;
        let row_bytes = usize::try_from(region.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(CaptureError::InvalidRegion)?;
        let mut rgba = Vec::with_capacity(
            row_bytes
                .checked_mul(
                    usize::try_from(region.height).map_err(|_| CaptureError::InvalidRegion)?,
                )
                .ok_or(CaptureError::InvalidRegion)?,
        );
        let source_row_bytes = usize::try_from(self.pixel_width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(CaptureError::InvalidFrame("frame dimensions are too large"))?;
        let source_x = usize::try_from(region.x)
            .ok()
            .and_then(|x| x.checked_mul(4))
            .ok_or(CaptureError::InvalidRegion)?;
        for row in region.y..region.y + region.height {
            let start = usize::try_from(row)
                .ok()
                .and_then(|row| row.checked_mul(source_row_bytes))
                .and_then(|offset| offset.checked_add(source_x))
                .ok_or(CaptureError::InvalidRegion)?;
            rgba.extend_from_slice(&self.rgba[start..start + row_bytes]);
        }

        let top_left = self.input_point(PixelPoint {
            x: region.x,
            y: region.y,
        })?;
        let bottom_right = self.input_point(PixelPoint {
            x: region.x + region.width - 1,
            y: region.y + region.height - 1,
        })?;
        let input_bounds = InputBounds {
            x: top_left.0,
            y: top_left.1,
            width: u32::try_from(bottom_right.0 - top_left.0 + 1)
                .map_err(|_| CaptureError::InvalidRegion)?,
            height: u32::try_from(bottom_right.1 - top_left.1 + 1)
                .map_err(|_| CaptureError::InvalidRegion)?,
        };
        Self::new(
            self.display_id.clone(),
            region.width,
            region.height,
            input_bounds,
            rgba,
        )
    }
}

fn map_axis(
    pixel: u32,
    pixel_extent: u32,
    input_origin: i32,
    input_extent: u32,
) -> CaptureResult<i32> {
    if pixel_extent == 0 || input_extent == 0 {
        return Err(CaptureError::InvalidFrame(
            "frame dimensions must be positive",
        ));
    }
    if pixel_extent == 1 || input_extent == 1 {
        return Ok(input_origin);
    }
    let offset = u64::from(pixel)
        .checked_mul(u64::from(input_extent - 1))
        .ok_or(CaptureError::InvalidRegion)?
        / u64::from(pixel_extent - 1);
    let offset = i32::try_from(offset).map_err(|_| CaptureError::InvalidRegion)?;
    input_origin
        .checked_add(offset)
        .ok_or(CaptureError::InvalidRegion)
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
pub(crate) fn decode_pixel_value(bytes: &[u8], little_endian: bool) -> u32 {
    if little_endian {
        bytes
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (index * 8))
            })
    } else {
        bytes
            .iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
    }
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
pub(crate) fn normalize_masked_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((u64::from(value) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

#[cfg(any(test, target_os = "windows"))]
pub(crate) fn bgra_to_rgba(bytes: &mut [u8]) {
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
}

fn validate_region(region: PixelRegion, width: u32, height: u32) -> CaptureResult<()> {
    if region.width == 0 || region.height == 0 {
        return Err(CaptureError::InvalidRegion);
    }
    let right = region
        .x
        .checked_add(region.width)
        .ok_or(CaptureError::InvalidRegion)?;
    let bottom = region
        .y
        .checked_add(region.height)
        .ok_or(CaptureError::InvalidRegion)?;
    if right > width || bottom > height {
        return Err(CaptureError::InvalidRegion);
    }
    Ok(())
}

pub type CaptureResult<T> = Result<T, CaptureError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    PermissionDenied,
    DisplayNotFound,
    InvalidRegion,
    InvalidFrame(&'static str),
    Unavailable(&'static str),
    Platform(String),
}

impl FmtDisplay for CaptureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => write!(formatter, "screen capture permission is denied"),
            Self::DisplayNotFound => write!(formatter, "display was not found"),
            Self::InvalidRegion => write!(formatter, "capture region is invalid"),
            Self::InvalidFrame(message) => write!(formatter, "capture frame is invalid: {message}"),
            Self::Unavailable(message) => {
                write!(formatter, "screen capture is unavailable: {message}")
            }
            Self::Platform(message) => write!(formatter, "screen capture failed: {message}"),
        }
    }
}

impl Error for CaptureError {}

/// Capture displays in the same coordinate space used by the input backend.
pub trait Screen {
    fn displays(&mut self) -> CaptureResult<Vec<Display>>;

    fn capture(&mut self, display: &DisplayId) -> CaptureResult<CaptureFrame>;

    fn capture_primary(&mut self) -> CaptureResult<CaptureFrame> {
        let display = self
            .displays()?
            .into_iter()
            .find(|display| display.primary)
            .ok_or(CaptureError::DisplayNotFound)?;
        self.capture(&display.id)
    }

    fn capture_region(
        &mut self,
        display: &DisplayId,
        region: PixelRegion,
    ) -> CaptureResult<CaptureFrame> {
        self.capture(display)?.crop(region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> CaptureFrame {
        CaptureFrame::new(
            DisplayId("primary".into()),
            4,
            2,
            InputBounds {
                x: 10,
                y: 20,
                width: 2,
                height: 2,
            },
            (0_u8..32).collect(),
        )
        .unwrap()
    }

    #[test]
    fn maps_screenshot_edges_to_input_edges() {
        let frame = frame();
        assert_eq!(
            frame.input_point(PixelPoint { x: 0, y: 0 }).unwrap(),
            (10, 20)
        );
        assert_eq!(
            frame.input_point(PixelPoint { x: 3, y: 1 }).unwrap(),
            (11, 21)
        );
    }

    #[test]
    fn crop_preserves_pixels_and_input_mapping() {
        let cropped = frame()
            .crop(PixelRegion {
                x: 2,
                y: 0,
                width: 2,
                height: 2,
            })
            .unwrap();
        assert_eq!(cropped.pixel_width, 2);
        assert_eq!(cropped.pixel_height, 2);
        assert_eq!(cropped.input_bounds.x, 10);
        assert_eq!(cropped.input_bounds.y, 20);
        assert_eq!(cropped.input_bounds.width, 2);
        assert_eq!(cropped.input_bounds.height, 2);
        assert_eq!(
            cropped.rgba,
            [8, 9, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 30, 31]
        );
    }

    #[test]
    fn rejects_mismatched_payloads_and_out_of_bounds_regions() {
        assert_eq!(
            CaptureFrame::new(
                DisplayId("primary".into()),
                1,
                1,
                InputBounds {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                vec![0; 3],
            )
            .unwrap_err(),
            CaptureError::InvalidFrame("RGBA payload length does not match the frame dimensions")
        );
        assert_eq!(
            frame()
                .crop(PixelRegion {
                    x: 3,
                    y: 0,
                    width: 2,
                    height: 1,
                })
                .unwrap_err(),
            CaptureError::InvalidRegion
        );
    }

    #[test]
    fn decodes_native_pixels_and_visual_masks() {
        assert_eq!(
            decode_pixel_value(&[0x33, 0x22, 0x11, 0], true),
            0x0011_2233
        );
        assert_eq!(
            decode_pixel_value(&[0, 0x11, 0x22, 0x33], false),
            0x0011_2233
        );
        let pixel = 0x00AA_8040;
        assert_eq!(normalize_masked_channel(pixel, 0x00FF_0000), 0xAA);
        assert_eq!(normalize_masked_channel(pixel, 0x0000_FF00), 0x80);
        assert_eq!(normalize_masked_channel(pixel, 0x0000_00FF), 0x40);
        let mut bgra = [0x40, 0x80, 0xAA, 0];
        bgra_to_rgba(&mut bgra);
        assert_eq!(bgra, [0xAA, 0x80, 0x40, 255]);
    }
}
