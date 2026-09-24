/// Index into `Document::assets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AssetId(pub usize);

/// An embedded binary asset (image, object payload). Bytes are always
/// retained so the document stays self-contained; total retained bytes are
/// capped by the fixed `max_asset_total_bytes` limit at parse time.
#[derive(Debug, Clone)]
pub struct Asset {
    /// This asset's own index, so a detached `Asset` still identifies itself.
    pub id: AssetId,
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Package part or stream the asset came from, for provenance.
    pub origin_part: String,
    /// The payload, exactly as stored in the source.
    pub bytes: Vec<u8>,
}

impl Asset {
    /// A file extension for [`Asset::media_type`], without the dot: `png`,
    /// `jpg`, `svg`, ... and `bin` for types without a common one.
    pub fn extension(&self) -> &'static str {
        match self.media_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            "image/tiff" => "tif",
            "image/svg+xml" => "svg",
            "image/webp" => "webp",
            "image/avif" => "avif",
            "image/jp2" => "jp2",
            "image/emf" | "image/x-emf" => "emf",
            "image/wmf" | "image/x-wmf" => "wmf",
            "application/pdf" => "pdf",
            _ => "bin",
        }
    }
}
