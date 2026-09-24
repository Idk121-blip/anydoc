//! Image XObjects as document assets.
//!
//! pdf-inspector marks where each image is drawn (`![Image: Im0](image)`)
//! by its resource name; this module finds that XObject on the page and
//! keeps its bytes in a form a browser can show. JPEG streams (`DCTDecode`)
//! pass through untouched; raw samples (uncompressed or `FlateDecode`) are
//! re-encoded as PNG, with an `SMask` becoming the alpha channel. Encodings
//! nothing downstream decodes (JBIG2, CCITT fax) stay unavailable.

use crate::error::ConvertError;
use crate::model::ImageSource;
use crate::package::limits;
use crate::shared::assets::AssetSink;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};

/// Form XObjects searched below a page for a nested image name.
const MAX_FORM_DEPTH: usize = 4;

/// Largest image, in pixels, re-encoded from raw samples.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

pub(super) struct Images<'d> {
    doc: Option<&'d Document>,
    pages: BTreeMap<u32, ObjectId>,
    sink: AssetSink,
    by_name: HashMap<(u32, String), ImageSource>,
    /// The first fatal error (the retained-bytes cap), surfaced after
    /// parsing since the resolver itself cannot fail.
    error: Option<ConvertError>,
}

impl<'d> Images<'d> {
    pub(super) fn new(doc: Option<&'d Document>) -> Self {
        Images {
            doc,
            pages: doc.map(Document::get_pages).unwrap_or_default(),
            sink: AssetSink::new(),
            by_name: HashMap::new(),
            error: None,
        }
    }

    /// The source for the image named `name` on 1-indexed `page`.
    pub(super) fn resolve(&mut self, page: u32, name: &str) -> ImageSource {
        let key = (page, name.to_string());
        if let Some(source) = self.by_name.get(&key) {
            return source.clone();
        }
        let source = self.load(page, name).unwrap_or(ImageSource::Unavailable);
        self.by_name.insert(key, source.clone());
        source
    }

    fn load(&mut self, page: u32, name: &str) -> Option<ImageSource> {
        if self.error.is_some() {
            return None;
        }
        let doc = self.doc?;
        let page_id = *self.pages.get(&page)?;
        let (inline, ids) = doc.get_page_resources(page_id).ok()?;
        let resources =
            inline.into_iter().chain(ids.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
        let (id, stream) = resources
            .into_iter()
            .find_map(|r| find_image(doc, r, name.as_bytes(), 0, &mut HashSet::new()))?;
        let (media_type, bytes) = encode(doc, stream)?;
        let origin = format!("{} {} R", id.0, id.1);
        match self.sink.add(media_type.to_string(), origin, &bytes) {
            Ok(asset) => Some(ImageSource::Asset(asset)),
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }

    pub(super) fn finish(self) -> Result<Vec<crate::model::Asset>, ConvertError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.sink.assets),
        }
    }
}

/// The image XObject named `name` in `resources`, or in a form XObject
/// drawn from them.
fn find_image<'a>(
    doc: &'a Document,
    resources: &'a Dictionary,
    name: &[u8],
    depth: usize,
    // Forms already searched: shared or cyclic forms are searched once.
    seen: &mut HashSet<ObjectId>,
) -> Option<(ObjectId, &'a Stream)> {
    let xobjects = resources.get(b"XObject").ok().and_then(|o| deref(doc, o).as_dict().ok())?;
    if let Ok(Object::Reference(id)) = xobjects.get(name)
        && let Ok(stream) = doc.get_object(*id).and_then(Object::as_stream)
        && subtype(stream) == Some(b"Image".as_slice())
    {
        return Some((*id, stream));
    }
    if depth >= MAX_FORM_DEPTH {
        return None;
    }
    for (_, object) in xobjects.iter() {
        if let Object::Reference(id) = object
            && !seen.insert(*id)
        {
            continue;
        }
        let Ok(stream) = deref(doc, object).as_stream() else { continue };
        if subtype(stream) != Some(b"Form".as_slice()) {
            continue;
        }
        let Some(inner) =
            stream.dict.get(b"Resources").ok().and_then(|r| deref(doc, r).as_dict().ok())
        else {
            continue;
        };
        if let Some(found) = find_image(doc, inner, name, depth + 1, seen) {
            return Some(found);
        }
    }
    None
}

fn deref<'a>(doc: &'a Document, object: &'a Object) -> &'a Object {
    doc.dereference(object).map(|(_, o)| o).unwrap_or(object)
}

fn subtype(stream: &Stream) -> Option<&[u8]> {
    stream.dict.get(b"Subtype").and_then(Object::as_name).ok()
}

fn filters(doc: &Document, dict: &Dictionary) -> Vec<Vec<u8>> {
    match dict.get(b"Filter").map(|f| deref(doc, f)) {
        Ok(Object::Name(name)) => vec![name.clone()],
        Ok(Object::Array(items)) => {
            items.iter().filter_map(|i| deref(doc, i).as_name().ok().map(<[u8]>::to_vec)).collect()
        }
        _ => Vec::new(),
    }
}

fn int(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    dict.get(key).ok().and_then(|o| deref(doc, o).as_i64().ok())
}

/// A browser-viewable encoding of the image: its media type and bytes.
fn encode(doc: &Document, stream: &Stream) -> Option<(&'static str, Vec<u8>)> {
    let dict = &stream.dict;
    let filters = filters(doc, dict);
    match filters.last().map(Vec::as_slice) {
        Some(b"DCTDecode" | b"DCT") => {
            let bytes =
                unfilter(doc, stream, &filters[..filters.len() - 1], limits::MAX_ENTRY_BYTES)?;
            bytes.starts_with(&[0xFF, 0xD8]).then_some(("image/jpeg", bytes))
        }
        Some(b"JPXDecode") => Some(("image/jp2", stream.content.clone())),
        Some(b"JBIG2Decode" | b"CCITTFaxDecode" | b"CCF" | b"RunLengthDecode" | b"RL") => None,
        _ => {
            let raster = Raster::read(doc, stream, true)?;
            Some(("image/png", raster.to_png()?))
        }
    }
}

/// Undo the lossless filters (none, or Flate with an optional predictor),
/// reading at most `cap` bytes.
fn unfilter(doc: &Document, stream: &Stream, filters: &[Vec<u8>], cap: u64) -> Option<Vec<u8>> {
    match filters {
        [] => Some(stream.content.clone()),
        [f] if f == b"FlateDecode" || f == b"Fl" => {
            let mut out = Vec::new();
            ZlibDecoder::new(stream.content.as_slice())
                .take(cap)
                .read_to_end(&mut out)
                .ok()
                .or_else(
                    // Truncated streams are common; keep what inflated.
                    || (!out.is_empty()).then_some(0),
                )?;
            let parms = stream
                .dict
                .get(b"DecodeParms")
                .ok()
                .map(|p| deref(doc, p))
                .and_then(|p| match p {
                    Object::Array(a) => a.first().map(|o| deref(doc, o)),
                    other => Some(other),
                })
                .and_then(|p| p.as_dict().ok());
            match parms {
                Some(parms) => unpredict(doc, parms, out),
                None => Some(out),
            }
        }
        _ => None,
    }
}

/// Reverse a PNG (10-15) or TIFF (2) predictor.
fn unpredict(doc: &Document, parms: &Dictionary, data: Vec<u8>) -> Option<Vec<u8>> {
    let predictor = int(doc, parms, b"Predictor").unwrap_or(1);
    if predictor < 2 {
        return Some(data);
    }
    let colors = int(doc, parms, b"Colors").unwrap_or(1).clamp(1, 32) as usize;
    let bpc = int(doc, parms, b"BitsPerComponent").unwrap_or(8).clamp(1, 16) as usize;
    let columns = int(doc, parms, b"Columns").unwrap_or(1).clamp(1, 1 << 24) as usize;
    let bpp = (colors * bpc).div_ceil(8);
    let row = (colors * bpc * columns).div_ceil(8);
    if predictor == 2 {
        if bpc != 8 {
            return None;
        }
        let mut data = data;
        for line in data.chunks_mut(row) {
            for i in bpp..line.len() {
                line[i] = line[i].wrapping_add(line[i - bpp]);
            }
        }
        return Some(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        if chunk.len() < 2 {
            break;
        }
        let kind = chunk[0];
        let mut line = chunk[1..].to_vec();
        line.resize(row, 0);
        for i in 0..row {
            let left = if i >= bpp { line[i - bpp] } else { 0 };
            let up = prev[i];
            let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
            line[i] = line[i].wrapping_add(match kind {
                1 => left,
                2 => up,
                3 => ((left as u16 + up as u16) / 2) as u8,
                4 => paeth(left, up, up_left),
                _ => 0,
            });
        }
        out.extend_from_slice(&line);
        prev = line;
    }
    Some(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Raw samples in a PNG-representable layout.
struct Raster {
    width: u32,
    height: u32,
    bit_depth: u8,
    color: Color,
    /// Rows packed as PNG expects them, without filter bytes.
    data: Vec<u8>,
}

enum Color {
    Gray,
    GrayAlpha,
    Rgb,
    Rgba,
    Palette(Vec<[u8; 3]>),
}

impl Color {
    fn channels(&self) -> usize {
        match self {
            Color::Gray | Color::Palette(_) => 1,
            Color::GrayAlpha => 2,
            Color::Rgb => 3,
            Color::Rgba => 4,
        }
    }
}

/// A PDF color space, reduced to what re-encoding needs.
enum Space {
    Gray,
    Rgb,
    Cmyk,
    Indexed { base: Box<Space>, palette: Vec<u8> },
}

impl Space {
    fn components(&self) -> usize {
        match self {
            Space::Gray | Space::Indexed { .. } => 1,
            Space::Rgb => 3,
            Space::Cmyk => 4,
        }
    }

    fn read(doc: &Document, object: &Object, depth: usize) -> Option<Space> {
        if depth > 4 {
            return None;
        }
        match deref(doc, object) {
            Object::Name(name) => match name.as_slice() {
                b"DeviceGray" | b"CalGray" | b"G" => Some(Space::Gray),
                b"DeviceRGB" | b"CalRGB" | b"RGB" => Some(Space::Rgb),
                b"DeviceCMYK" | b"CMYK" => Some(Space::Cmyk),
                _ => None,
            },
            Object::Array(items) => {
                let family = deref(doc, items.first()?).as_name().ok()?;
                match family {
                    b"ICCBased" => {
                        let profile = deref(doc, items.get(1)?).as_stream().ok()?;
                        match int(doc, &profile.dict, b"N")? {
                            1 => Some(Space::Gray),
                            3 => Some(Space::Rgb),
                            4 => Some(Space::Cmyk),
                            _ => None,
                        }
                    }
                    b"CalGray" => Some(Space::Gray),
                    b"CalRGB" => Some(Space::Rgb),
                    b"Indexed" | b"I" => {
                        let base = Space::read(doc, items.get(1)?, depth + 1)?;
                        let lookup = match deref(doc, items.get(3)?) {
                            Object::String(bytes, _) => bytes.clone(),
                            Object::Stream(stream) => {
                                let f = filters(doc, &stream.dict);
                                unfilter(doc, stream, &f, 1 << 20)?
                            }
                            _ => return None,
                        };
                        Some(Space::Indexed { base: Box::new(base), palette: lookup })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let k = 255 - k as u16;
    let channel = |v: u8| ((255 - v as u16) * k / 255) as u8;
    [channel(c), channel(m), channel(y)]
}

impl Raster {
    /// `with_alpha` folds in the image's soft mask; a mask's own mask is
    /// never read, so a mask naming itself cannot recurse.
    fn read(doc: &Document, stream: &Stream, with_alpha: bool) -> Option<Raster> {
        let dict = &stream.dict;
        let width = u32::try_from(int(doc, dict, b"Width")?).ok().filter(|w| *w > 0)?;
        let height = u32::try_from(int(doc, dict, b"Height")?).ok().filter(|h| *h > 0)?;
        if width as u64 * height as u64 > MAX_PIXELS {
            return None;
        }
        let mask = dict.get(b"ImageMask").ok().and_then(|o| o.as_bool().ok()).unwrap_or(false);
        let (space, bpc) = if mask {
            (Space::Gray, 1)
        } else {
            let space = Space::read(doc, dict.get(b"ColorSpace").ok()?, 0)?;
            (space, int(doc, dict, b"BitsPerComponent").unwrap_or(8))
        };
        if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
            return None;
        }
        let bpc = bpc as usize;
        let row = (width as usize * space.components() * bpc).div_ceil(8);
        let expected = row * height as usize;
        let mut data = unfilter(doc, stream, &filters(doc, dict), expected as u64 + 1)?;
        if data.len() < expected {
            data.resize(expected, 0);
        }
        data.truncate(expected);
        // `/Decode [1 0]` on a one-component image inverts it; stencil
        // masks paint where samples are 0 unless decoded the other way.
        let inverted = dict
            .get(b"Decode")
            .ok()
            .and_then(|d| deref(doc, d).as_array().ok())
            .and_then(|d| {
                Some(
                    deref(doc, d.first()?).as_float().ok()?
                        > deref(doc, d.get(1)?).as_float().ok()?,
                )
            })
            .unwrap_or(false);
        if inverted && matches!(space, Space::Gray) {
            for byte in &mut data {
                *byte = !*byte;
            }
        }
        let mut raster = match space {
            Space::Gray => Raster { width, height, bit_depth: bpc as u8, color: Color::Gray, data },
            Space::Rgb => Raster { width, height, bit_depth: bpc as u8, color: Color::Rgb, data },
            Space::Cmyk => {
                if bpc != 8 {
                    return None;
                }
                let rgb = data
                    .chunks_exact(4)
                    .flat_map(|p| cmyk_to_rgb(p[0], p[1], p[2], p[3]))
                    .collect();
                Raster { width, height, bit_depth: 8, color: Color::Rgb, data: rgb }
            }
            Space::Indexed { base, palette } => {
                if bpc > 8 {
                    return None;
                }
                let entries: Vec<[u8; 3]> = match *base {
                    Space::Gray => palette.iter().map(|&g| [g, g, g]).collect(),
                    Space::Rgb => palette.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
                    Space::Cmyk => palette
                        .chunks_exact(4)
                        .map(|c| cmyk_to_rgb(c[0], c[1], c[2], c[3]))
                        .collect(),
                    Space::Indexed { .. } => return None,
                };
                if entries.is_empty() {
                    return None;
                }
                let mut entries = entries;
                entries.truncate(256);
                Raster { width, height, bit_depth: bpc as u8, color: Color::Palette(entries), data }
            }
        };
        if let Some(alpha) = dict.get(b"SMask").ok().and_then(|s| deref(doc, s).as_stream().ok())
            && with_alpha
        {
            raster.add_alpha(doc, alpha);
        }
        Some(raster)
    }

    /// Fold a same-size 8-bit gray soft mask in as the alpha channel.
    fn add_alpha(&mut self, doc: &Document, mask: &Stream) {
        if self.bit_depth != 8 || !matches!(self.color, Color::Gray | Color::Rgb) {
            return;
        }
        let Some(alpha) = Raster::read(doc, mask, false) else { return };
        if alpha.width != self.width
            || alpha.height != self.height
            || alpha.bit_depth != 8
            || !matches!(alpha.color, Color::Gray)
        {
            return;
        }
        let channels = self.color.channels();
        let mut data = Vec::with_capacity(self.data.len() / channels * (channels + 1));
        for (pixel, a) in self.data.chunks_exact(channels).zip(&alpha.data) {
            data.extend_from_slice(pixel);
            data.push(*a);
        }
        self.data = data;
        self.color = if channels == 1 { Color::GrayAlpha } else { Color::Rgba };
    }

    fn to_png(&self) -> Option<Vec<u8>> {
        let row =
            (self.width as usize * self.color.channels() * self.bit_depth as usize).div_ceil(8);
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        for line in self.data.chunks(row) {
            z.write_all(&[0]).ok()?;
            z.write_all(line).ok()?;
        }
        let idat = z.finish().ok()?;
        let color_type = match self.color {
            Color::Gray => 0,
            Color::Rgb => 2,
            Color::Palette(_) => 3,
            Color::GrayAlpha => 4,
            Color::Rgba => 6,
        };
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[self.bit_depth, color_type, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        if let Color::Palette(entries) = &self.color {
            let plte: Vec<u8> = entries.iter().flatten().copied().collect();
            chunk(&mut png, b"PLTE", &plte);
        }
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);
        Some(png)
    }
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        table
    });
    !bytes.iter().fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_png_reference() {
        // The CRC of an empty IEND chunk is fixed by the PNG spec.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn png_predictor_rows_decode() {
        let doc = Document::new();
        let mut parms = Dictionary::new();
        parms.set("Predictor", 12);
        parms.set("Columns", 2);
        // Row 1 "none" [1, 2]; row 2 "up" adds the row above: [1+1, 2+2].
        let decoded = unpredict(&doc, &parms, vec![0, 1, 2, 2, 1, 2]).unwrap();
        assert_eq!(decoded, vec![1, 2, 2, 4]);
    }

    #[test]
    fn a_mask_naming_itself_does_not_recurse() {
        let mut doc = Document::with_version("1.4");
        let id = doc.new_object_id();
        let mut dict = Dictionary::new();
        dict.set("Subtype", Object::Name(b"Image".to_vec()));
        dict.set("Width", 1);
        dict.set("Height", 1);
        dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
        dict.set("BitsPerComponent", 8);
        dict.set("SMask", Object::Reference(id));
        doc.objects.insert(id, Object::Stream(Stream::new(dict, vec![7])));
        let stream = doc.get_object(id).unwrap().as_stream().unwrap();
        let (media_type, png) = encode(&doc, stream).unwrap();
        assert_eq!(media_type, "image/png");
        assert!(png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn cmyk_black_is_black() {
        assert_eq!(cmyk_to_rgb(0, 0, 0, 255), [0, 0, 0]);
        assert_eq!(cmyk_to_rgb(0, 0, 0, 0), [255, 255, 255]);
    }
}
