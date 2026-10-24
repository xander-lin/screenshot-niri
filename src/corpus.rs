//! Reading a captured corpus.
//!
//! A corpus is a directory that `capture-frames` wrote: `frame-NNNN.png` files
//! plus a `session.json` manifest holding each frame's screen offset and the
//! direction hint. The offline tools (`replay-frames`, `truth-frames`) both read
//! it, so the reader lives here rather than in either binary — a manifest change
//! must not be able to drift between them.
//!
//! Nothing here talks to Wayland or the stitcher: it decodes files and produces
//! luminance planes, which is what every pixel-level check compares.

use std::error::Error;
use std::path::{Path, PathBuf};

use crate::image::Image;
use crate::stitch::FrameScreenOffset;
use crate::wayland::selection::LongDirection;

/// One captured frame: where it came from and what the user was doing.
pub struct FrameEntry {
    pub path: PathBuf,
    /// On-screen position of the captured region, in logical coordinates.
    pub offset: FrameScreenOffset,
    pub direction: Option<LongDirection>,
}

/// Loads a corpus directory, in capture order.
///
/// With a manifest the order and offsets come from it; without one every PNG is
/// taken in name order with a zero offset (a corpus stripped down to images is
/// still replayable, just without geometry information).
pub fn load_frames(dir: &Path) -> Result<Vec<FrameEntry>, Box<dyn Error>> {
    let manifest = dir.join("session.json");
    if manifest.exists() {
        let text = std::fs::read_to_string(&manifest)?;
        return parse_manifest(dir, &text);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "png").unwrap_or(false))
        .collect();
    entries.sort();
    Ok(entries
        .into_iter()
        .map(|path| FrameEntry { path, offset: FrameScreenOffset { x: 0, y: 0 }, direction: None })
        .collect())
}

/// Minimal reader for the manifest `capture-frames` writes.
///
/// Deliberately not a general JSON parser: the file is produced by this
/// repository and the fields are flat.
fn parse_manifest(dir: &Path, text: &str) -> Result<Vec<FrameEntry>, Box<dyn Error>> {
    let mut frames = Vec::new();
    // Each frame is one object containing a `file` key. Slicing from that key to
    // the closing brace keeps the reader independent of indentation, so both the
    // compact writer in `capture-frames` and a pretty-printed manifest parse the
    // same way.
    let mut rest = text;
    while let Some(start) = rest.find("\"file\"") {
        let tail = &rest[start..];
        let Some(end) = tail.find('}') else { break };
        let object = &tail[..end];
        rest = &tail[end..];

        let file = field(object, "file").ok_or("frame entry without a file")?;
        let ox = field(object, "offset_x").and_then(|value| value.parse::<i32>().ok()).unwrap_or(0);
        let oy = field(object, "offset_y").and_then(|value| value.parse::<i32>().ok()).unwrap_or(0);
        let direction = match field(object, "direction").as_deref() {
            Some("down") => Some(LongDirection::Down),
            Some("up") => Some(LongDirection::Up),
            Some("left") => Some(LongDirection::Left),
            Some("right") => Some(LongDirection::Right),
            _ => None,
        };
        frames.push(FrameEntry {
            path: dir.join(file),
            offset: FrameScreenOffset { x: ox, y: oy },
            direction,
        });
    }
    Ok(frames)
}

fn field(line: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\": ");
    let start = line.find(&key)? + key.len();
    let rest = &line[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Decodes one PNG into the BGRA layout the rest of the pipeline uses.
///
/// `Xrgb8888` with the alpha byte set to 255, matching what screencopy hands
/// the live path, so a pixel-level check on a replayed corpus compares the same
/// bytes the stitcher saw in production.
pub fn load_png(path: &Path) -> Result<Image, Box<dyn Error>> {
    let file = std::fs::File::open(path)?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG reports no output size")?];
    let info = reader.next_frame(&mut buf)?;
    let (width, height) = (info.width, info.height);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4usize,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        other => return Err(format!("unsupported PNG colour type {other:?}").into()),
    };
    let stride = width * 4;
    let mut data = vec![0u8; stride as usize * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let s = (y * width as usize + x) * channels;
            let d = y * stride as usize + x * 4;
            let (r, g, b) = match channels {
                4 | 3 => (buf[s], buf[s + 1], buf[s + 2]),
                _ => (buf[s], buf[s], buf[s]),
            };
            // Match the in-memory layout the rest of the tool expects: B,G,R,X.
            data[d] = b;
            data[d + 1] = g;
            data[d + 2] = r;
            data[d + 3] = 255;
        }
    }
    Ok(Image { width, height, stride, format: wayland_client::protocol::wl_shm::Format::Xrgb8888, data })
}

/// One luminance byte per pixel, so a pixel-level search touches a quarter of
/// the memory the BGRA buffer would and compares a single channel.
pub fn luminance(image: &Image) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.width as usize * image.height as usize);
    for y in 0..image.height as usize {
        let row = y * image.stride as usize;
        for x in 0..image.width as usize {
            let o = row + x * 4;
            if o + 2 >= image.data.len() {
                out.push(0);
                continue;
            }
            // Stored as B, G, R, X.
            let b = u32::from(image.data[o]);
            let g = u32::from(image.data[o + 1]);
            let r = u32::from(image.data[o + 2]);
            out.push(((r * 77 + g * 150 + b * 29) >> 8) as u8);
        }
    }
    out
}
