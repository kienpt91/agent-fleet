//! What a file at a commit looks like to a person: text with its line offsets, an image, a
//! placeholder for something that is not shown (binary, too large), a symbolic link's target text
//! or a submodule's commit. Contents are bytes from a repository and are never trusted: text is
//! decoded leniently (invalid UTF-8 is replaced and the person is told), long lines are clipped
//! with a marker, the amount of text is bounded, and a link is only ever described, never followed.

use super::limits::{
    MAX_FILE_VIEW_BYTES, MAX_FILE_VIEW_LINES, MAX_IMAGE_BYTES, MAX_LINE_CHARS, MAX_PATH_BYTES,
    SNIFF_BYTES,
};
use super::path::RepoPath;
use super::tree::{display_text, EntryKind, ModeClass};
use crate::ids::Sha;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpeg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
        }
    }

    /// The only content types the blob API ever sends.
    pub fn content_type(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }
}

/// What the start of a file says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeClass {
    Text,
    Image(ImageInfo),
    Binary,
}

impl TypeClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TypeClass::Text => "text",
            TypeClass::Image(_) => "image",
            TypeClass::Binary => "binary",
        }
    }
}

fn starts(bytes: &[u8], magic: &[u8]) -> bool {
    bytes.get(..magic.len()) == Some(magic)
}

/// The format and the declared size of an image. The size is what the file's own header says; it
/// is read here, before anything is handed to a browser, because a few kilobytes can declare
/// a canvas of billions of pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
}

/// What size of image the viewer shows. Always within the hard caps, whatever is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLimits {
    max_side: u32,
    max_pixels: u64,
}

impl ImageLimits {
    /// Most pixels along one side, whatever the operator configures.
    pub const HARD_MAX_SIDE: u32 = 16_384;
    /// Most pixels in all, whatever the operator configures.
    pub const HARD_MAX_PIXELS: u64 = 64_000_000;
    pub const DEFAULT_MAX_SIDE: u32 = 8192;
    pub const DEFAULT_MAX_PIXELS: u64 = 32_000_000;

    /// Limits clamped to `1..=HARD_MAX_*`.
    pub fn new(max_side: u32, max_pixels: u64) -> ImageLimits {
        ImageLimits {
            max_side: max_side.clamp(1, Self::HARD_MAX_SIDE),
            max_pixels: max_pixels.clamp(1, Self::HARD_MAX_PIXELS),
        }
    }

    pub fn max_side(&self) -> u32 {
        self.max_side
    }

    pub fn max_pixels(&self) -> u64 {
        self.max_pixels
    }

    /// Whether an image of this size may be shown.
    pub fn allows(&self, width: u32, height: u32) -> bool {
        width > 0
            && height > 0
            && width <= self.max_side
            && height <= self.max_side
            && u64::from(width)
                .checked_mul(u64::from(height))
                .is_some_and(|p| p <= self.max_pixels)
    }
}

impl Default for ImageLimits {
    fn default() -> ImageLimits {
        ImageLimits::new(Self::DEFAULT_MAX_SIDE, Self::DEFAULT_MAX_PIXELS)
    }
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    let h = b.get(at..at + 2)?;
    Some(u32::from(*h.first()?) << 8 | u32::from(*h.get(1)?))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(be16(b, at)? << 16 | be16(b, at + 2)?)
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    let h = b.get(at..at + 2)?;
    Some(u32::from(*h.get(1)?) << 8 | u32::from(*h.first()?))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let h = b.get(at..at + 3)?;
    Some(u32::from(*h.get(2)?) << 16 | u32::from(*h.get(1)?) << 8 | u32::from(*h.first()?))
}

/// Segments of a JPEG that are looked at before giving up on finding the frame header.
const MAX_JPEG_SEGMENTS: usize = 4096;

/// The size a JPEG declares in its first frame header (SOF), found by walking the segments.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    for _ in 0..MAX_JPEG_SEGMENTS {
        // markers are 0xFF, optional fill 0xFF bytes, then the marker code
        while *b.get(i)? != 0xff {
            i += 1;
            if i > b.len() {
                return None;
            }
        }
        while *b.get(i + 1)? == 0xff {
            i += 1;
        }
        let m = *b.get(i + 1)?;
        match m {
            // not a marker at all (an escaped 0xFF is only valid inside scan data)
            0x00 => return None,
            // markers without a length
            0x01 | 0xd0..=0xd7 => i += 2,
            // end of image, or the scan starts before any frame header: there is none
            0xd9 | 0xda => return None,
            // start of frame (not DHT, JPG, DAC)
            0xc0..=0xcf if !matches!(m, 0xc4 | 0xc8 | 0xcc) => {
                let len = be16(b, i + 2)?;
                if len < 8 {
                    return None;
                }
                return Some((be16(b, i + 7)?, be16(b, i + 5)?));
            }
            _ => {
                let len = usize::try_from(be16(b, i + 2)?).ok()?;
                if len < 2 {
                    return None;
                }
                i = i.checked_add(2)?.checked_add(len)?;
            }
        }
    }
    None
}

/// The format and declared size of `bytes` by magic number and a structural check of the header,
/// or `None`. A truncated or inconsistent header, a size of zero and anything that merely contains
/// a magic number later on are not images (the browser is never handed them). SVG is text and is
/// never an image here: it can carry script. The size is not limited here, see [`ImageLimits`].
pub fn image_info(bytes: &[u8]) -> Option<ImageInfo> {
    let (kind, width, height) = if starts(bytes, b"\x89PNG\r\n\x1a\n") {
        // the first chunk must be IHDR with its 13 bytes of data; size at 16..24
        if bytes.len() < 33
            || bytes.get(8..12) != Some(&[0, 0, 0, 13][..])
            || bytes.get(12..16) != Some(&b"IHDR"[..])
        {
            return None;
        }
        (ImageKind::Png, be32(bytes, 16)?, be32(bytes, 20)?)
    } else if starts(bytes, b"\xff\xd8\xff") {
        let (w, h) = jpeg_size(bytes)?;
        (ImageKind::Jpeg, w, h)
    } else if starts(bytes, b"GIF87a") || starts(bytes, b"GIF89a") {
        // header, then the 7-byte logical screen descriptor (little endian size)
        if bytes.len() < 13 {
            return None;
        }
        (ImageKind::Gif, le16(bytes, 6)?, le16(bytes, 8)?)
    } else if starts(bytes, b"RIFF") && bytes.get(8..12) == Some(&b"WEBP"[..]) {
        let (w, h) = match bytes.get(12..16)? {
            b"VP8 " => {
                // frame tag (3 bytes), start code 9d 01 2a, then 14-bit width and height
                if bytes.get(23..26) != Some(&[0x9d, 0x01, 0x2a][..]) {
                    return None;
                }
                (le16(bytes, 26)? & 0x3fff, le16(bytes, 28)? & 0x3fff)
            }
            b"VP8L" => {
                // signature 0x2f, then width-1 and height-1 in 14 bits each
                if *bytes.get(20)? != 0x2f {
                    return None;
                }
                let bits = le16(bytes, 21)? | le16(bytes, 23)? << 16;
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"VP8X" => {
                // flags and reserved bytes, then the canvas size minus one in 24 bits each
                (le24(bytes, 24)? + 1, le24(bytes, 27)? + 1)
            }
            _ => return None,
        };
        (ImageKind::Webp, w, h)
    } else {
        return None;
    };
    (width > 0 && height > 0).then_some(ImageInfo {
        kind,
        width,
        height,
    })
}

/// The image format of `bytes`, see [`image_info`].
pub fn image_kind(bytes: &[u8]) -> Option<ImageKind> {
    image_info(bytes).map(|i| i.kind)
}

/// Text, an image or binary, from the first [`SNIFF_BYTES`] bytes: an image by [`image_kind`],
/// binary when a NUL byte is there (as git decides), text otherwise.
pub fn sniff(bytes: &[u8]) -> TypeClass {
    if let Some(info) = image_info(bytes) {
        return TypeClass::Image(info);
    }
    let prefix = bytes.get(..SNIFF_BYTES).unwrap_or(bytes);
    if prefix.contains(&0) {
        TypeClass::Binary
    } else {
        TypeClass::Text
    }
}

/// Paths that are rendered as Markdown (by name only; whether the file is small enough is the
/// caller's decision).
pub fn is_markdown_path(path: &RepoPath) -> bool {
    matches!(
        path.extension().as_deref(),
        Some("md" | "markdown" | "mdown" | "mkd")
    )
}

/// Decoded text with the byte offset at which each line starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextView {
    /// The shown lines joined with `\n`, line endings removed.
    text: String,
    /// Start of each shown line in `text`.
    line_starts: Vec<usize>,
    /// Lines of the whole file, shown or not.
    pub line_count: usize,
    /// There are more lines than were kept.
    pub truncated: bool,
    /// Invalid UTF-8 was replaced.
    pub lossy: bool,
    /// At least one line ended with CRLF.
    pub crlf: bool,
    /// Lines that were clipped with a marker.
    pub clipped: usize,
}

impl TextView {
    /// Decodes `bytes`: lenient UTF-8, a leading byte order mark dropped, CRLF and LF line
    /// endings removed, at most [`MAX_FILE_VIEW_LINES`] lines of at most [`MAX_LINE_CHARS`]
    /// characters (a longer line ends with a marker that says how much was left out).
    pub fn from_bytes(bytes: &[u8]) -> TextView {
        let decoded = String::from_utf8_lossy(bytes);
        let lossy = matches!(decoded, std::borrow::Cow::Owned(_));
        let body = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);
        let mut view = TextView {
            text: String::new(),
            line_starts: Vec::new(),
            line_count: 0,
            truncated: false,
            lossy,
            crlf: false,
            clipped: 0,
        };
        let mut pieces = body.split('\n').peekable();
        while let Some(piece) = pieces.next() {
            // a final empty piece is what the last line ending leaves behind, not a line
            if pieces.peek().is_none() && piece.is_empty() {
                break;
            }
            view.line_count += 1;
            let line = match piece.strip_suffix('\r') {
                Some(l) => {
                    view.crlf = true;
                    l
                }
                None => piece,
            };
            if view.line_starts.len() >= MAX_FILE_VIEW_LINES {
                view.truncated = true;
                continue;
            }
            if !view.line_starts.is_empty() {
                view.text.push('\n');
            }
            view.line_starts.push(view.text.len());
            let (kept, rest) = clip_chars(line, MAX_LINE_CHARS);
            view.text.push_str(kept);
            if rest > 0 {
                view.clipped += 1;
                view.text
                    .push_str(&format!(" \u{2026}[{rest} more characters]"));
            }
        }
        view
    }

    /// Lines that were kept.
    pub fn shown(&self) -> usize {
        self.line_starts.len()
    }

    /// The `index`-th kept line (0-based).
    pub fn line(&self, index: usize) -> Option<&str> {
        let start = *self.line_starts.get(index)?;
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.text.len(), |next| next.saturating_sub(1));
        self.text.get(start..end)
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        (0..self.shown()).filter_map(|i| self.line(i))
    }

    /// The text of the kept lines.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The first `max` characters of `s` and how many characters were left out.
fn clip_chars(s: &str, max: usize) -> (&str, usize) {
    match s.char_indices().nth(max) {
        None => (s, 0),
        Some((at, _)) => (
            s.get(..at).unwrap_or(s),
            s.get(at..).map_or(0, |r| r.chars().count()),
        ),
    }
}

/// What is shown of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileContent {
    Text(TextView),
    /// Binary data: only its size and class are shown.
    Binary,
    /// An image of a kind and size the viewer may show; the bytes come from the blob API.
    Image(ImageInfo),
    /// An image whose declared size is above the limits: a placeholder, the bytes are never served.
    ImageTooBig(ImageInfo, ImageLimits),
    /// Larger than the viewer shows.
    TooLarge,
    /// A symbolic link: its target as text.
    Symlink {
        target: String,
        /// The target as a repository path, when it is a plain relative path inside the
        /// repository (never followed by anything here).
        target_path: Option<RepoPath>,
    },
    /// A gitlink: the commit of the submodule, which is not in this repository.
    Submodule {
        commit: Option<Sha>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileView {
    pub path: RepoPath,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mode: ModeClass,
    pub content: FileContent,
    /// The path names a Markdown document and the text is small enough to render.
    pub markdown: bool,
}

impl FileView {
    /// Builds the view of an entry that is not a directory. `bytes` is the content when it was
    /// read (`None` when it was not: too large to read). `oid` is the object id git printed
    /// (for a submodule, the commit it records).
    pub fn build(
        path: &RepoPath,
        kind: EntryKind,
        mode: ModeClass,
        size: Option<u64>,
        oid: &str,
        bytes: Option<&[u8]>,
        images: &ImageLimits,
    ) -> FileView {
        let content = match kind {
            EntryKind::Submodule => FileContent::Submodule {
                commit: Sha::parse(oid).ok(),
            },
            EntryKind::Dir => FileContent::TooLarge,
            EntryKind::Symlink => match bytes {
                Some(b) => {
                    let target = std::str::from_utf8(b.get(..MAX_PATH_BYTES).unwrap_or(b)).ok();
                    FileContent::Symlink {
                        target: display_text(b, MAX_PATH_BYTES),
                        target_path: target.and_then(|t| path.parent().resolve_literal(t)),
                    }
                }
                None => FileContent::TooLarge,
            },
            EntryKind::File => match bytes {
                None => FileContent::TooLarge,
                Some(b) => match sniff(b) {
                    TypeClass::Image(i) if b.len() > MAX_IMAGE_BYTES => {
                        let _ = i;
                        FileContent::Binary
                    }
                    TypeClass::Image(i) if images.allows(i.width, i.height) => {
                        FileContent::Image(i)
                    }
                    TypeClass::Image(i) => FileContent::ImageTooBig(i, *images),
                    TypeClass::Binary => FileContent::Binary,
                    TypeClass::Text if b.len() > MAX_FILE_VIEW_BYTES => FileContent::TooLarge,
                    TypeClass::Text => FileContent::Text(TextView::from_bytes(b)),
                },
            },
        };
        let markdown = matches!(content, FileContent::Text(_))
            && is_markdown_path(path)
            && size.is_some_and(|s| s <= super::limits::MAX_MARKDOWN_BYTES as u64);
        FileView {
            path: path.clone(),
            kind,
            size,
            mode,
            content,
            markdown,
        }
    }

    /// `text`, `image`, `binary` or `unknown` (a file that was too large to look at).
    pub fn class(&self) -> &'static str {
        match &self.content {
            FileContent::Text(_) => "text",
            FileContent::Image(_) | FileContent::ImageTooBig(..) => "image",
            FileContent::Binary => "binary",
            FileContent::TooLarge | FileContent::Symlink { .. } | FileContent::Submodule { .. } => {
                "unknown"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn png_sized(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    fn png() -> Vec<u8> {
        png_sized(1, 1)
    }

    /// A JPEG with a few segments before the frame header, which declares `w` by `h`.
    fn jpeg_sized(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec();
        v.extend_from_slice(b"\xff\xfe\0\x05abc");
        v.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        v
    }

    fn jpeg() -> Vec<u8> {
        jpeg_sized(1, 1)
    }

    fn gif_sized(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0]);
        v
    }

    fn gif() -> Vec<u8> {
        gif_sized(1, 1)
    }

    fn webp_lossy(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"RIFF\x1a\0\0\0WEBPVP8 \x0e\0\0\0\x10\0\0\x9d\x01\x2a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v
    }

    fn webp_lossless(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF\x1a\0\0\0WEBPVP8L\x05\0\0\0\x2f".to_vec();
        let bits = (w - 1) | ((h - 1) << 14);
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v
    }

    fn webp_extended(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF\x1a\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        v.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        v.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        v.extend_from_slice(&[0; 4]);
        v
    }

    fn webp() -> Vec<u8> {
        webp_lossy(1, 1)
    }

    #[test]
    fn declared_sizes_are_read_from_every_format() {
        let size = |b: &[u8]| image_info(b).map(|i| (i.kind, i.width, i.height));
        assert_eq!(size(&png_sized(640, 480)), Some((ImageKind::Png, 640, 480)));
        assert_eq!(
            size(&jpeg_sized(1920, 1080)),
            Some((ImageKind::Jpeg, 1920, 1080))
        );
        assert_eq!(size(&gif_sized(300, 200)), Some((ImageKind::Gif, 300, 200)));
        assert_eq!(
            size(&webp_lossy(800, 600)),
            Some((ImageKind::Webp, 800, 600))
        );
        assert_eq!(
            size(&webp_lossless(4000, 3000)),
            Some((ImageKind::Webp, 4000, 3000))
        );
        assert_eq!(
            size(&webp_extended(16384, 16384)),
            Some((ImageKind::Webp, 16384, 16384))
        );
        // the largest values each header can hold are read exactly, without overflow
        assert_eq!(
            size(&png_sized(u32::MAX, u32::MAX)),
            Some((ImageKind::Png, u32::MAX, u32::MAX))
        );
        assert_eq!(
            size(&gif_sized(u16::MAX, u16::MAX)),
            Some((ImageKind::Gif, 65535, 65535))
        );
        assert_eq!(
            size(&jpeg_sized(u16::MAX, u16::MAX)),
            Some((ImageKind::Jpeg, 65535, 65535))
        );
        assert_eq!(
            size(&webp_extended(1 << 24, 1 << 24)),
            Some((ImageKind::Webp, 1 << 24, 1 << 24))
        );
    }

    #[test]
    fn a_size_of_zero_or_a_header_without_one_is_not_an_image() {
        assert_eq!(image_info(&png_sized(0, 5)), None);
        assert_eq!(image_info(&png_sized(5, 0)), None);
        assert_eq!(image_info(&gif_sized(0, 1)), None);
        assert_eq!(image_info(&gif_sized(1, 0)), None);
        assert_eq!(image_info(&jpeg_sized(0, 1)), None);
        assert_eq!(image_info(&webp_lossy(0, 3)), None);
        // a JPEG whose scan starts before any frame header, or that ends first
        let mut no_frame = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec();
        no_frame.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 0x3f, 0]);
        assert_eq!(image_info(&no_frame), None);
        assert_eq!(image_info(&no_frame[..20]), None);
        // a frame header that is too short to hold a size, or cut in the middle of it
        let short = [0xff, 0xd8, 0xff, 0xc0, 0, 4, 8, 0];
        assert_eq!(image_info(&short), None);
        let cut = jpeg_sized(10, 10);
        assert_eq!(image_info(&cut[..cut.len() - 14]), None);
        // a segment length that points past the end, or backwards
        let far = [0xff, 0xd8, 0xff, 0xe0, 0xff, 0xff, 1, 2, 3];
        assert_eq!(image_info(&far), None);
        let back = [0xff, 0xd8, 0xff, 0xe0, 0, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(image_info(&back), None);
        // WebP whose chunk is too short for its canvas, or has the wrong signature
        assert_eq!(image_info(&webp_extended(10, 10)[..28]), None);
        assert_eq!(image_info(&webp_lossless(10, 10)[..24]), None);
        let mut bad_start = webp_lossy(10, 10);
        bad_start[24] = 0;
        assert_eq!(image_info(&bad_start), None);
        let mut bad_sig = webp_lossless(10, 10);
        bad_sig[20] = 0x2e;
        assert_eq!(image_info(&bad_sig), None);
        // a PNG whose first chunk claims another length, or is not IHDR
        let mut long = png_sized(10, 10);
        long[11] = 14;
        assert_eq!(image_info(&long), None);
    }

    #[test]
    fn many_fill_bytes_and_segments_cannot_make_the_jpeg_walk_long() {
        // thousands of tiny segments before the frame header: given up on, not walked to the end
        let mut v = vec![0xff, 0xd8];
        for _ in 0..10_000 {
            v.extend_from_slice(&[0xff, 0xfe, 0, 2]);
        }
        v.extend_from_slice(&[
            0xff, 0xc0, 0, 17, 8, 0, 1, 0, 1, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1,
        ]);
        assert_eq!(image_info(&v), None);
        // endless fill bytes
        let fill = [vec![0xff, 0xd8, 0xff], vec![0xff; 100_000]].concat();
        let start = std::time::Instant::now();
        assert_eq!(image_info(&fill), None);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn limits_are_clamped_and_checked_with_exact_arithmetic() {
        let l = ImageLimits::default();
        assert_eq!((l.max_side(), l.max_pixels()), (8192, 32_000_000));
        assert!(
            l.allows(8192, 3906) && !l.allows(8192, 3907),
            "32 megapixels"
        );
        assert!(!l.allows(8193, 1) && !l.allows(1, 8193));
        assert!(!l.allows(0, 5) && !l.allows(5, 0));
        assert!(!l.allows(u32::MAX, u32::MAX), "no overflow");
        let big = ImageLimits::new(u32::MAX, u64::MAX);
        assert_eq!(
            (big.max_side(), big.max_pixels()),
            (ImageLimits::HARD_MAX_SIDE, ImageLimits::HARD_MAX_PIXELS),
            "configuration cannot lift the hard caps"
        );
        let small = ImageLimits::new(0, 0);
        assert_eq!((small.max_side(), small.max_pixels()), (1, 1));
        assert!(small.allows(1, 1) && !small.allows(2, 1));
    }

    #[test]
    fn an_image_above_the_limits_is_a_placeholder_never_an_image() {
        let build = |bytes: &[u8], limits: &ImageLimits| {
            FileView::build(
                &RepoPath::parse("x.png").unwrap(),
                EntryKind::File,
                ModeClass::Regular,
                Some(bytes.len() as u64),
                &"a".repeat(40),
                Some(bytes),
                limits,
            )
        };
        let l = ImageLimits::default();
        // a few dozen bytes that declare a 40000 x 40000 canvas, in every format
        for b in [
            png_sized(40_000, 40_000),
            gif_sized(60_000, 60_000),
            jpeg_sized(60_000, 60_000),
            webp_extended(40_000, 40_000),
            webp_lossless(16_384, 16_384),
            png_sized(8193, 1),
            png_sized(6000, 6000),
        ] {
            let v = build(&b, &l);
            assert!(
                matches!(v.content, FileContent::ImageTooBig(..)),
                "{:?}",
                image_info(&b)
            );
            assert_eq!(v.class(), "image");
        }
        let ok = build(&png_sized(4000, 4000), &l);
        assert!(matches!(ok.content, FileContent::Image(i) if i.width == 4000));
        // a size that is fine under one limit and refused under another
        assert!(matches!(
            build(&png_sized(5000, 5000), &ImageLimits::new(8192, 64_000_000)).content,
            FileContent::Image(_)
        ));
        assert!(matches!(
            build(&png_sized(5000, 5000), &ImageLimits::new(4096, 64_000_000)).content,
            FileContent::ImageTooBig(..)
        ));
        // a header that does not parse is no image at all
        assert_eq!(build(&png_sized(0, 0), &l).content, FileContent::Binary);
    }

    #[test]
    fn images_are_recognized_by_magic_number_and_a_minimal_header() {
        assert_eq!(image_kind(&png()), Some(ImageKind::Png));
        assert_eq!(image_kind(&jpeg()), Some(ImageKind::Jpeg));
        assert_eq!(image_kind(&gif()), Some(ImageKind::Gif));
        assert_eq!(image_kind(&webp()), Some(ImageKind::Webp));
        let mut gif87 = gif();
        gif87[4] = b'7';
        assert_eq!(image_kind(&gif87), Some(ImageKind::Gif));
        for (kind, name, mime) in [
            (ImageKind::Png, "png", "image/png"),
            (ImageKind::Jpeg, "jpeg", "image/jpeg"),
            (ImageKind::Gif, "gif", "image/gif"),
            (ImageKind::Webp, "webp", "image/webp"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.content_type(), mime);
        }
    }

    #[test]
    fn truncated_and_forged_images_are_not_images() {
        // cut inside the header
        assert_eq!(image_kind(&png()[..20]), None);
        assert_eq!(image_kind(&png()[..8]), None);
        assert_eq!(image_kind(&jpeg()[..5]), None);
        assert_eq!(image_kind(&gif()[..8]), None);
        assert_eq!(image_kind(&webp()[..14]), None);
        assert_eq!(image_kind(b""), None);
        // a magic number with the wrong structure behind it
        let mut bad_png = png();
        bad_png[15] = b'X';
        assert_eq!(image_kind(&bad_png), None);
        let mut bad_webp = webp();
        bad_webp[12] = b'Z';
        assert_eq!(image_kind(&bad_webp), None);
        let mut bad_jpeg = jpeg();
        bad_jpeg[3] = 0x00;
        assert_eq!(image_kind(&bad_jpeg), None);
        // a magic number that is not at the start
        let mut late = b"<html>".to_vec();
        late.extend_from_slice(&png());
        assert_eq!(image_kind(&late), None);
        // svg, with and without script, is text
        for svg in [
            &b"<svg xmlns='x'><script>alert(1)</script></svg>"[..],
            b"<?xml version=\"1.0\"?><svg onload=\"alert(1)\"/>",
            b"\xef\xbb\xbf<svg/>",
        ] {
            assert_eq!(image_kind(svg), None);
            assert_eq!(sniff(svg), TypeClass::Text);
        }
    }

    #[test]
    fn a_polyglot_is_an_image_by_its_start_and_nothing_else() {
        // a GIF whose "pixels" are HTML: it is a gif (served as one, with nosniff and a
        // sandbox) and its text is never interpreted
        let mut p = gif();
        p.extend_from_slice(b"<script>alert(1)</script>");
        assert_eq!(
            sniff(&p),
            TypeClass::Image(ImageInfo {
                kind: ImageKind::Gif,
                width: 1,
                height: 1
            })
        );
        // html that starts like text and ends like a gif is text
        let mut q = b"<script>alert(1)</script>".to_vec();
        q.extend_from_slice(&gif());
        assert_eq!(image_kind(&q), None);
    }

    #[test]
    fn text_and_binary_are_told_apart_by_a_nul_near_the_start() {
        assert_eq!(sniff(b"hello\nworld\n"), TypeClass::Text);
        assert_eq!(sniff(b""), TypeClass::Text);
        assert_eq!(sniff(b"ab\0cd"), TypeClass::Binary);
        assert_eq!(sniff(&[0xff, 0xfe, b'a', 0]), TypeClass::Binary);
        let mut late = vec![b'a'; SNIFF_BYTES];
        late.push(0);
        assert_eq!(
            sniff(&late),
            TypeClass::Text,
            "a NUL after the sniffed prefix"
        );
        assert_eq!(TypeClass::Text.as_str(), "text");
        assert_eq!(TypeClass::Binary.as_str(), "binary");
        assert_eq!(
            TypeClass::Image(ImageInfo {
                kind: ImageKind::Png,
                width: 1,
                height: 1
            })
            .as_str(),
            "image"
        );
    }

    #[test]
    fn markdown_is_recognized_by_extension() {
        for yes in ["README.md", "a/b.MD", "x.markdown", "y.mdown", "z.mkd"] {
            assert!(is_markdown_path(&RepoPath::parse(yes).unwrap()), "{yes}");
        }
        for no in ["a.txt", "md", "a.md.bak", ".md", "a.mdx"] {
            assert!(!is_markdown_path(&RepoPath::parse(no).unwrap()), "{no}");
        }
    }

    #[test]
    fn text_keeps_its_lines_and_their_offsets() {
        let t = TextView::from_bytes(b"one\ntwo\n\nfour\n");
        assert_eq!(t.line_count, 4);
        assert_eq!(t.shown(), 4);
        assert_eq!(t.lines().collect::<Vec<_>>(), ["one", "two", "", "four"]);
        assert_eq!(t.line(1), Some("two"));
        assert_eq!(t.line(4), None);
        assert!(!t.truncated && !t.lossy && !t.crlf);
        assert_eq!(t.text(), "one\ntwo\n\nfour");
        // no final newline
        let t = TextView::from_bytes(b"a\nb");
        assert_eq!(t.lines().collect::<Vec<_>>(), ["a", "b"]);
        // empty file: no lines; a lone newline is one empty line
        assert_eq!(TextView::from_bytes(b"").shown(), 0);
        let nl = TextView::from_bytes(b"\n");
        assert_eq!((nl.shown(), nl.line(0)), (1, Some("")));
    }

    #[test]
    fn crlf_bom_and_invalid_utf8_are_handled_and_reported() {
        let t = TextView::from_bytes(b"\xef\xbb\xbfa\r\nb\r\nc");
        assert_eq!(t.lines().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(t.crlf && !t.lossy);
        let t = TextView::from_bytes(b"ok \xff\xfe bad\n");
        assert!(t.lossy);
        assert_eq!(t.line(0), Some("ok \u{fffd}\u{fffd} bad"));
        // a lone carriage return stays in the text (the page makes it visible)
        let t = TextView::from_bytes(b"a\rb\n");
        assert_eq!(t.line(0), Some("a\rb"));
        assert!(!t.crlf);
        // the control characters of the text are not removed here either
        let t = TextView::from_bytes(b"x\x1b[31my\n");
        assert_eq!(t.line(0), Some("x\u{1b}[31my"));
    }

    #[test]
    fn long_lines_are_clipped_with_a_marker_and_long_files_are_cut() {
        let long = "é".repeat(MAX_LINE_CHARS + 25);
        let t = TextView::from_bytes(format!("{long}\nshort\n").as_bytes());
        assert_eq!(t.clipped, 1);
        let first = t.line(0).unwrap();
        assert!(first.starts_with(&"é".repeat(MAX_LINE_CHARS)));
        assert!(first.ends_with("[25 more characters]"), "{first}");
        assert_eq!(t.line(1), Some("short"));
        let exact = "x".repeat(MAX_LINE_CHARS);
        assert_eq!(TextView::from_bytes(exact.as_bytes()).clipped, 0);

        let many = "l\n".repeat(MAX_FILE_VIEW_LINES + 7);
        let t = TextView::from_bytes(many.as_bytes());
        assert!(t.truncated);
        assert_eq!(t.shown(), MAX_FILE_VIEW_LINES);
        assert_eq!(t.line_count, MAX_FILE_VIEW_LINES + 7);
        assert!(t.line(MAX_FILE_VIEW_LINES).is_none());
    }

    fn view(path: &str, kind: EntryKind, bytes: Option<&[u8]>, size: u64) -> FileView {
        FileView::build(
            &RepoPath::parse(path).unwrap(),
            kind,
            ModeClass::Regular,
            Some(size),
            &"a".repeat(40),
            bytes,
            &ImageLimits::default(),
        )
    }

    #[test]
    fn a_file_view_is_text_binary_image_or_too_large() {
        let v = view("a.txt", EntryKind::File, Some(b"hi\n"), 3);
        assert!(matches!(v.content, FileContent::Text(_)));
        assert_eq!(v.class(), "text");
        assert!(!v.markdown);
        let v = view("a.bin", EntryKind::File, Some(b"a\0b"), 3);
        assert_eq!((&v.content, v.class()), (&FileContent::Binary, "binary"));
        let v = view("a.png", EntryKind::File, Some(&png()), 33);
        assert!(matches!(v.content, FileContent::Image(i) if i.kind == ImageKind::Png));
        assert_eq!(v.class(), "image");
        let v = view("a.rs", EntryKind::File, None, 99_999_999);
        assert_eq!((&v.content, v.class()), (&FileContent::TooLarge, "unknown"));
        // text above the text limit but within what was read
        let big = vec![b'a'; MAX_FILE_VIEW_BYTES + 1];
        let v = view("big.txt", EntryKind::File, Some(&big), big.len() as u64);
        assert_eq!(v.content, FileContent::TooLarge);
        // an image above the image limit is not served
        let mut huge = png();
        huge.resize(MAX_IMAGE_BYTES + 1, 0);
        let v = view("huge.png", EntryKind::File, Some(&huge), huge.len() as u64);
        assert_eq!(v.content, FileContent::Binary);
    }

    #[test]
    fn markdown_is_offered_for_small_text_only() {
        let v = view("README.md", EntryKind::File, Some(b"# hi\n"), 5);
        assert!(v.markdown);
        let v = view("README.md", EntryKind::File, Some(b"# hi\n"), 5_000_000);
        assert!(!v.markdown);
        let v = view("README.md", EntryKind::File, Some(b"\0\0"), 2);
        assert!(!v.markdown);
    }

    #[test]
    fn a_symlink_is_described_never_followed() {
        let v = view("docs/link", EntryKind::Symlink, Some(b"../README.md"), 12);
        match &v.content {
            FileContent::Symlink {
                target,
                target_path,
            } => {
                assert_eq!(target, "../README.md");
                assert_eq!(
                    target_path.as_ref().map(RepoPath::as_str),
                    Some("README.md")
                );
            }
            other => panic!("{other:?}"),
        }
        for (target, expect_path) in [
            (&b"/etc/passwd"[..], false),
            (b"../../outside", false),
            (b"a/b", true),
            (b"x?y#z", true),
            (b"%2e%2e/x", true),
            (b"..", false),
            (b"../docs", true),
            (b"\xff\xfe", false),
            (b"a\\b", false),
        ] {
            let v = view("docs/link", EntryKind::Symlink, Some(target), 1);
            match v.content {
                FileContent::Symlink { target_path, .. } => {
                    assert_eq!(target_path.is_some(), expect_path, "{target:?}")
                }
                other => panic!("{other:?}"),
            }
        }
        // the target text is made visible, not interpreted
        let v = view("l", EntryKind::Symlink, Some(b"a\x1b[0m\n"), 5);
        match v.content {
            FileContent::Symlink { target, .. } => assert_eq!(target, "a\\u{1b}[0m\\u{a}"),
            other => panic!("{other:?}"),
        }
        // a link whose target could not be read is a placeholder
        let v = view("l", EntryKind::Symlink, None, 5_000_000);
        assert_eq!(v.content, FileContent::TooLarge);
    }

    #[test]
    fn a_submodule_shows_its_commit_only() {
        let sha = "b".repeat(40);
        let v = FileView::build(
            &RepoPath::parse("vendor/lib").unwrap(),
            EntryKind::Submodule,
            ModeClass::Other,
            None,
            &sha,
            None,
            &ImageLimits::default(),
        );
        assert_eq!(
            v.content,
            FileContent::Submodule {
                commit: Some(Sha::parse(&sha).unwrap())
            }
        );
        let v = FileView::build(
            &RepoPath::parse("vendor/lib").unwrap(),
            EntryKind::Submodule,
            ModeClass::Other,
            None,
            "not a sha",
            None,
            &ImageLimits::default(),
        );
        assert_eq!(v.content, FileContent::Submodule { commit: None });
        assert_eq!(v.class(), "unknown");
    }

    /// Whatever the bytes are, the text view stays within its caps.
    #[test]
    fn random_bytes_never_break_the_caps() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let len = usize::try_from(next() % 4000).unwrap_or(0);
            let bytes: Vec<u8> = (0..len)
                .map(|_| match next() % 6 {
                    0 => b'\n',
                    1 => b'\r',
                    2 => 0xff,
                    3 => b'a',
                    4 => 0xc3,
                    _ => 0x80,
                })
                .collect();
            let t = TextView::from_bytes(&bytes);
            assert!(t.shown() <= MAX_FILE_VIEW_LINES);
            assert!(t.lines().all(|l| l.chars().count() <= MAX_LINE_CHARS + 40));
            assert!(t.line_count >= t.shown());
            assert_eq!(t.lines().count(), t.shown());
        }
    }

    /// Random damage to valid headers: never a panic, never a size of zero, and a size that comes
    /// out is what a clean parse of the same bytes gives.
    #[test]
    fn damaged_headers_never_panic_or_yield_a_zero_size() {
        let seeds = [
            png_sized(300, 200),
            jpeg_sized(300, 200),
            gif_sized(300, 200),
            webp_lossy(300, 200),
            webp_lossless(300, 200),
            webp_extended(300, 200),
        ];
        let mut state = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut b = seeds[usize::try_from(next() % seeds.len() as u64).unwrap()].clone();
            for _ in 0..(1 + next() % 4) {
                let at = usize::try_from(next() % b.len() as u64).unwrap();
                match next() % 3 {
                    0 => b[at] = u8::try_from(next() % 256).unwrap(),
                    1 => b.truncate(at),
                    _ => b.insert(at, u8::try_from(next() % 256).unwrap()),
                }
                if b.is_empty() {
                    break;
                }
            }
            if let Some(i) = image_info(&b) {
                assert!(i.width > 0 && i.height > 0);
                let _ = ImageLimits::default().allows(i.width, i.height);
            }
        }
    }
}
