use super::{ColorMode, Document, LayerContent, PSD_ESTIMATE_FIXED_OVERHEAD, PSD_ESTIMATE_LAYER_OVERHEAD, PSD_MAX_FILE_SIZE, SampleType, Surface, Version};

/// How much of a PSD conversion the merged composite weighs, in layer records: rendering it
/// (all layers composited) and encoding its planes cost about two full-canvas layers.
pub(super) const MERGED_WEIGHT: f32 = 2.0;

/// The part of the merged composite's share filled once its bands are rendered and converted; the
/// rest is compressing its planes.
pub(super) const MERGED_RENDERED: f32 = 0.8;

/// Adds headroom for compressed channel streams and their per-row length tables.
pub(super) fn estimate_encoded_size(raw_bytes: u64, rows: u64, structural_bytes: u64) -> Option<u64> {
    raw_bytes.checked_add(raw_bytes.checked_add(63)?.checked_div(64)?)?.checked_add(rows.checked_mul(8)?)?.checked_add(structural_bytes)
}

fn add_plane_estimate(raw_bytes: &mut u64, rows: &mut u64, width: u64, height: u64, channels: u64, bytes_per_sample: u64) -> Option<()> {
    let plane_bytes = width.checked_mul(height)?.checked_mul(channels)?.checked_mul(bytes_per_sample)?;
    let plane_rows = height.checked_mul(channels)?;
    *raw_bytes = raw_bytes.checked_add(plane_bytes)?;
    *rows = rows.checked_add(plane_rows)?;
    Some(())
}

pub(super) fn exceeds_psd_size_limit(estimated_size: Option<u64>) -> bool {
    estimated_size.is_none_or(|size| size > PSD_MAX_FILE_SIZE)
}

fn surface_dimensions(surface: &Surface) -> Option<(u64, u64)> {
    let bounds = surface.content_bounds();
    Some((u64::from(bounds.width()), u64::from(bounds.height())))
}

#[derive(Default)]
struct Estimate {
    raw_bytes: u64,
    rows: u64,
    structural_bytes: u64,
}

impl Estimate {
    fn new() -> Self {
        Estimate { structural_bytes: PSD_ESTIMATE_FIXED_OVERHEAD, ..Default::default() }
    }

    fn add_plane(&mut self, width: u64, height: u64, channels: u64, sample_bytes: u64) -> Option<()> {
        add_plane_estimate(&mut self.raw_bytes, &mut self.rows, width, height, channels, sample_bytes)
    }

    fn add_structure(&mut self, bytes: usize) -> Option<()> {
        self.structural_bytes = self.structural_bytes.checked_add(u64::try_from(bytes).ok()?)?;
        Some(())
    }
}

pub(crate) fn estimate_psd_size(doc: &Document) -> Option<u64> {
    let mut estimate = Estimate::new();
    let width = u64::from(doc.size.width);
    let height = u64::from(doc.size.height);
    if doc.mode == ColorMode::Multichannel {
        estimate_multichannel(doc, &mut estimate, width, height)?;
    } else {
        estimate_layered(doc, &mut estimate, width, height)?;
    }
    estimate_document_metadata(doc, &mut estimate)?;
    estimate_encoded_size(estimate.raw_bytes, estimate.rows, estimate.structural_bytes)
}

fn estimate_multichannel(doc: &Document, estimate: &mut Estimate, width: u64, height: u64) -> Option<()> {
    let channels = u64::try_from(doc.channels.len().clamp(1, 56)).ok()?;
    let sample = if doc.depth == SampleType::F32 { SampleType::U16 } else { doc.depth };
    estimate.add_plane(width, height, channels, u64::try_from(sample.bytes()).ok()?)
}

fn estimate_layered(doc: &Document, estimate: &mut Estimate, width: u64, height: u64) -> Option<()> {
    let format = doc.pixel_format();
    let color_channels = u64::try_from(format.mode.color_channels()).ok()?;
    let sample_bytes = u64::try_from(format.sample.bytes()).ok()?;
    let layer_channels = color_channels.checked_add(1)?;
    for (_, _, layer) in doc.walk() {
        estimate_layer(layer, estimate, width, height, layer_channels, sample_bytes)?;
    }
    estimate_merged(doc, estimate, width, height, color_channels, sample_bytes, format.mode.color_channels())
}

fn estimate_layer(layer: &photocraft_doc::Layer, estimate: &mut Estimate, width: u64, height: u64, channels: u64, sample_bytes: u64) -> Option<()> {
    estimate.structural_bytes = estimate.structural_bytes.checked_add(PSD_ESTIMATE_LAYER_OVERHEAD)?.checked_add(u64::try_from(layer.name.len()).ok()?)?;
    for (_, data) in &layer.psd_blocks {
        estimate.add_structure(data.len())?;
    }
    if let LayerContent::Fill(fill) = &layer.content {
        let mut dims = (width, height);
        if let Some(cache) = &layer.fill_cache
            && cache.fill == *fill
        {
            let (w, h) = surface_dimensions(&cache.surface)?;
            dims.0 = dims.0.max(w);
            dims.1 = dims.1.max(h);
        }
        estimate.add_plane(dims.0, dims.1, channels, sample_bytes)?;
    } else if let Some(surface) = layer.surface() {
        let (w, h) = surface_dimensions(surface)?;
        estimate.add_plane(w, h, channels, sample_bytes)?;
    }
    if let Some(mask) = &layer.mask {
        let (w, h) = surface_dimensions(&mask.surface)?;
        estimate.add_plane(w, h, 1, sample_bytes)?;
    }
    if let LayerContent::Smart(smart) = &layer.content
        && let photocraft_doc::SmartSource::Embedded { bytes, .. } = &smart.source
    {
        estimate.add_structure(bytes.len())?;
    }
    Some(())
}

fn estimate_merged(
    doc: &Document,
    estimate: &mut Estimate,
    width: u64,
    height: u64,
    color_channels: u64,
    sample_bytes: u64,
    color_channel_count: usize,
) -> Option<()> {
    let max_extra_channels = 56usize.saturating_sub(color_channel_count + 1);
    let mut extra_channels = doc.channels.len().min(max_extra_channels);
    if doc.quick_mask.is_some() && extra_channels < max_extra_channels {
        extra_channels += 1;
    }
    let channels = color_channels.checked_add(1)?.checked_add(u64::try_from(extra_channels).ok()?)?;
    estimate.add_plane(width, height, channels, sample_bytes)?;
    for channel in doc.channels.iter().take(extra_channels) {
        estimate.add_structure(channel.name.len())?;
    }
    Some(())
}

fn estimate_document_metadata(doc: &Document, estimate: &mut Estimate) -> Option<()> {
    estimate.add_structure(doc.icc_profile.as_ref().map_or(0, |profile| profile.len()))?;
    estimate.add_structure(doc.metadata.xmp.as_deref().map_or(0, str::len))?;
    estimate.add_structure(doc.metadata.exif.as_ref().map_or(0, |data| data.len()))?;
    for (_, name, data) in &doc.metadata.psd_resources {
        estimate.add_structure(name.len())?;
        estimate.add_structure(data.len())?;
    }
    for (_, _, data) in &doc.metadata.psd_global_blocks {
        estimate.add_structure(data.len())?;
    }
    Some(())
}

/// The signature a kept tagged block is written with. `8B64` exists only in PSB files: a PSD
/// with an `8B64` block (kept from a PSB source, e.g. its global `FMsk` or `cinf`) is refused by
/// Photoshop as "not compatible with this version" (measured in Photoshop 27.11). The other way
/// round, a PSB block with an 8-byte length is `8B64`, as Photoshop writes it: an `8BIM` `lnk2`,
/// `pths` or `cinf` (kept from a PSD source, whose lengths the PSB widens) makes Photoshop read
/// the length as 4 bytes and refuse the whole file (#7937).
pub(crate) fn block_signature(sig: [u8; 4], key: [u8; 4], version: Version) -> [u8; 4] {
    match version {
        Version::Psd if sig == *b"8B64" => *b"8BIM",
        Version::Psb if sig == *b"8BIM" && photocraft_psd::tagged::uses_long_length(version, &key) => *b"8B64",
        _ => sig,
    }
}

pub(crate) fn psd_version(doc: &Document, force_psb: bool, size_exceeds_limit: bool) -> Version {
    let exceeds_dimensions = doc.size.width > 30_000 || doc.size.height > 30_000;
    if force_psb || exceeds_dimensions || size_exceeds_limit { Version::Psb } else { Version::Psd }
}

pub(crate) fn psd_size_exceeds_limit(doc: &Document) -> bool {
    exceeds_psd_size_limit(estimate_psd_size(doc))
}
