//! Tables and figures read from page geometry.
//!
//! pdf-inspector finds tables from text alignment and drawn cells, and gets
//! three common shapes wrong:
//!
//! - statistical tables ruled with horizontal lines only, whose header
//!   groups columns ("2017" over two columns, "2018" over three);
//! - two-column layouts split by a vertical rule, whose entries do not line
//!   up across the rule, with section titles running across both columns;
//! - charts and infographics, whose labels it lays out as table cells.
//!
//! Here the rules and shapes a page draws ([`geometry`]) locate those
//! regions, and the text inside them is rebuilt: tables with their column
//! groups as spans, figures as their labels. Callers replace what
//! pdf-inspector made of the same text.

use super::geometry::{Fill, PageGeometry, Rect, Rule};
use crate::model::{Block, Cell, GridBuilder, Inline, Style, Table, TableKind};
use crate::shared::header::resolve_header_rows;
use pdf_inspector::TextItem;

/// Horizontal rules shorter than this (points) are never table borders.
const MIN_HRULE: f32 = 40.0;
/// Vertical rules shorter than this are marks, not column separators.
const MIN_VRULE: f32 = 8.0;

/// A run of text on one baseline, with no column-sized gap inside it.
#[derive(Debug, Clone)]
pub(super) struct Chunk {
    pub x0: f32,
    pub x1: f32,
    /// Baseline.
    pub y: f32,
    pub size: f32,
    pub runs: Vec<(String, Style)>,
}

impl Chunk {
    pub fn text(&self) -> String {
        self.runs.iter().map(|(t, _)| t.as_str()).collect()
    }

    fn top(&self) -> f32 {
        self.y + self.size * 0.8
    }

    fn bottom(&self) -> f32 {
        self.y - self.size * 0.25
    }

    fn center_x(&self) -> f32 {
        (self.x0 + self.x1) / 2.0
    }

    fn center_y(&self) -> f32 {
        (self.top() + self.bottom()) / 2.0
    }

    fn bold(&self) -> bool {
        self.runs.iter().all(|(t, s)| s.bold || t.trim().is_empty())
    }

    fn push(&mut self, text: &str, style: Style) {
        match self.runs.last_mut() {
            Some((prev, prev_style)) if *prev_style == style => prev.push_str(text),
            _ => self.runs.push((text.to_string(), style)),
        }
    }

    fn append(&mut self, other: &Chunk) {
        let gap = other.x0 - self.x1;
        let needs_space = gap > self.size * 0.12
            && !self.text().ends_with(char::is_whitespace)
            && !other.text().starts_with(char::is_whitespace);
        if needs_space {
            self.push(" ", Style::PLAIN);
        }
        for (text, style) in &other.runs {
            self.push(text, *style);
        }
        self.x1 = self.x1.max(other.x1);
    }
}

/// What was found in one region of a page.
#[derive(Debug)]
pub(super) struct Found {
    pub page: u32,
    pub region: Rect,
    /// The blocks the region becomes, in order: captions, the table (or the
    /// figure's text), notes.
    pub blocks: Vec<Block>,
    /// Index of the table in `blocks`, when the region is a table.
    pub table: Option<usize>,
    /// Every word of the region, captions and notes included: what
    /// pdf-inspector's blocks for it are matched against.
    pub text: String,
    pub is_figure: bool,
    /// Nothing but running headers above the region: a table here may
    /// continue one from the previous page.
    pub at_top: bool,
    /// Nothing but notes and folios below the region.
    pub at_bottom: bool,
    pub has_caption: bool,
}

/// Chunks of a page's text, top to bottom and left to right.
pub(super) fn chunks(items: &[TextItem]) -> Vec<Chunk> {
    let mut items: Vec<&TextItem> = items
        .iter()
        .filter(|i| {
            matches!(i.item_type, pdf_inspector::types::ItemType::Text)
                && !i.text.trim().is_empty()
                && i.x.is_finite()
                && i.y.is_finite()
        })
        .collect();
    items.sort_by(|a, b| b.y.total_cmp(&a.y));
    // Baselines within a point and a half of each other are one line.
    let mut bands: Vec<Vec<&TextItem>> = Vec::new();
    for item in items {
        match bands.last_mut() {
            Some(band) if (band[0].y - item.y).abs() <= 1.5 => band.push(item),
            _ => bands.push(vec![item]),
        }
    }
    let mut out = Vec::new();
    for mut band in bands {
        band.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut current: Option<Chunk> = None;
        for item in band {
            let size = item.font_size.max(item.height).max(1.0);
            let style = Style {
                bold: item.is_bold,
                italic: item.is_italic,
                strike: item.is_strikeout,
                code: false,
            };
            let chunk = Chunk {
                x0: item.x,
                x1: item.x + item.width.max(0.0),
                y: item.y,
                size,
                runs: vec![(item.text.clone(), style)],
            };
            match &mut current {
                Some(c) if chunk.x0 - c.x1 <= c.size.max(size) * 0.4 && chunk.x0 >= c.x0 - 1.0 => {
                    c.append(&chunk);
                    c.size = c.size.max(size);
                }
                _ => {
                    if let Some(done) = current.take() {
                        out.push(done);
                    }
                    current = Some(chunk);
                }
            }
        }
        out.extend(current);
    }
    out
}

/// The page's text as lines, top to bottom.
pub(super) fn page_lines(chunks: &[Chunk]) -> Vec<super::tables::PageLine> {
    let all: Vec<usize> = (0..chunks.len()).collect();
    lines_of(chunks, &all)
        .into_iter()
        .map(|line| super::tables::PageLine {
            y: line.y,
            text: line.chunks.iter().map(|&i| chunks[i].text()).collect::<Vec<_>>().join(" "),
        })
        .collect()
}

/// Shapes or rules on a page past which it is left to pdf-inspector: the
/// region search compares them pairwise.
const MAX_SHAPES: usize = 1500;
/// Text chunks on a page past which the same holds.
const MAX_CHUNKS: usize = 20_000;

/// Whether the page draws enough for a table or a figure to be worth
/// reading its text again: two body-width rules, a column rule, or a
/// figure's worth of shapes.
pub(super) fn worth_reading(geometry: &PageGeometry) -> bool {
    let (head, foot) = (geometry.height * 0.93, geometry.height * 0.05);
    let hrules = geometry
        .hrules
        .iter()
        .filter(|r| r.len() >= MIN_HRULE && r.at < head && r.at > foot)
        .count();
    let vrules: f32 = geometry
        .vrules
        .iter()
        .filter(|r| r.len() >= MIN_VRULE && r.from < head && r.to > foot)
        .map(Rule::len)
        .sum();
    let shapes = geometry.fills.iter().filter(|f| !is_band(f)).count() + geometry.images.len();
    let curved = geometry
        .fills
        .iter()
        .any(|f| f.curved && f.rect.width() >= 40.0 && f.rect.height() >= 40.0);
    hrules >= 2 || vrules >= 40.0 || shapes >= 3 || curved
}

/// Regions of `page` that are tables or figures, with what they become.
pub(super) fn find(page: u32, geometry: &PageGeometry, chunks: &[Chunk]) -> Vec<Found> {
    let shapes = geometry.hrules.len()
        + geometry.vrules.len()
        + geometry.fills.len()
        + geometry.images.len();
    if shapes > MAX_SHAPES || chunks.len() > MAX_CHUNKS {
        return Vec::new();
    }
    let body_size = body_size(chunks);
    let mut used = vec![false; chunks.len()];
    let mut found = Vec::new();
    for region in figure_regions(geometry, chunks, &used) {
        if let Some(f) = figure(page, geometry, chunks, &mut used, region, body_size) {
            found.push(f);
        }
    }
    for candidate in table_regions(geometry, chunks, &used) {
        if let Some(f) = table(page, geometry, chunks, &mut used, candidate, body_size) {
            found.push(f);
        }
    }
    found.sort_by(|a, b| b.region.y1.total_cmp(&a.region.y1));
    found
}

/// The most common text size, weighted by length: the size of body text.
fn body_size(chunks: &[Chunk]) -> f32 {
    let mut weights: Vec<(i32, usize)> = Vec::new();
    for c in chunks {
        let key = (c.size * 2.0).round() as i32;
        let len = c.text().len();
        match weights.iter_mut().find(|(k, _)| *k == key) {
            Some((_, w)) => *w += len,
            None => weights.push((key, len)),
        }
    }
    weights.iter().max_by_key(|(_, w)| *w).map_or(10.0, |(k, _)| *k as f32 / 2.0)
}

// ---------------------------------------------------------------------------
// Figures

/// Filled shapes that belong to tables rather than figures: shading bands
/// behind a row or a title.
fn is_band(fill: &Fill) -> bool {
    !fill.curved && fill.rect.height() <= 30.0 && fill.rect.width() >= fill.rect.height() * 6.0
}

fn figure_regions(geometry: &PageGeometry, chunks: &[Chunk], used: &[bool]) -> Vec<Rect> {
    let page_area = geometry.width * geometry.height;
    let mut shapes: Vec<(Rect, bool)> = geometry
        .fills
        .iter()
        .filter(|f| !is_band(f) && f.rect.width() * f.rect.height() < page_area * 0.5)
        .map(|f| (f.rect, f.curved))
        .collect();
    shapes.extend(
        geometry
            .images
            .iter()
            .filter(|r| {
                r.width() >= 20.0 && r.height() >= 20.0 && r.width() * r.height() < page_area * 0.6
            })
            .map(|r| (*r, true)),
    );
    // Shapes that touch (within a few points) form one figure.
    let mut clusters: Vec<(Rect, usize, bool)> = Vec::new();
    for (rect, curved) in shapes {
        let mut merged = (rect, 1, curved);
        let mut i = 0;
        while i < clusters.len() {
            // Bars of one chart stand a bar's width apart at most.
            if clusters[i].0.expand(28.0).intersects(&merged.0) {
                let (r, n, c) = clusters.swap_remove(i);
                merged = (merged.0.union(&r), merged.1 + n, merged.2 || c);
                i = 0;
            } else {
                i += 1;
            }
        }
        clusters.push(merged);
    }
    clusters
        .into_iter()
        .filter(|(rect, count, curved)| {
            (*count >= 3 || (*curved && rect.width() >= 40.0 && rect.height() >= 40.0))
                && rect.width() >= 60.0
                && rect.height() >= 40.0
        })
        .map(|(rect, _, _)| rect)
        .filter(|rect| {
            // A shaded box around paragraphs is a sidebar, not a figure:
            // figures carry labels, not prose.
            let inside: Vec<&Chunk> = chunks
                .iter()
                .zip(used)
                .filter(|(c, u)| {
                    !**u && rect.expand(15.0).contains_point(c.center_x(), c.center_y())
                })
                .map(|(c, _)| c)
                .collect();
            let prose = inside
                .iter()
                .filter(|c| {
                    c.x1 - c.x0 > rect.width() * 0.6 && c.text().split_whitespace().count() >= 8
                })
                .count();
            inside.len() >= 2 && prose * 4 < inside.len()
        })
        .collect()
}

fn figure(
    page: u32,
    geometry: &PageGeometry,
    chunks: &[Chunk],
    used: &mut [bool],
    region: Rect,
    body_size: f32,
) -> Option<Found> {
    // Labels sit on the shapes or just around them (axis ticks, legends,
    // text set around a ring), each close to the next.
    let short = |c: &Chunk| c.text().split_whitespace().count() <= 6;
    let mut area = region.expand(15.0);
    let mut members: Vec<usize> = Vec::new();
    loop {
        let before = members.len();
        for i in 0..chunks.len() {
            let c = &chunks[i];
            if used[i] || members.contains(&i) {
                continue;
            }
            let rect = Rect { x0: c.x0, y0: c.bottom(), x1: c.x1, y1: c.top() };
            // Longer text belongs to the figure only on its shapes, and prose
            // never does.
            let prose =
                c.x1 - c.x0 > region.width() * 0.6 || c.text().split_whitespace().count() > 12;
            let on_shapes = region.expand(15.0).contains_point(c.center_x(), c.center_y());
            let near = short(c) && rect.intersects(&area.expand(c.size));
            if !prose && (on_shapes || near) {
                members.push(i);
                area = area.union(&rect);
            }
        }
        if members.len() == before || members.len() > 2000 {
            break;
        }
    }
    if members.len() < 2 {
        return None;
    }
    for &i in &members {
        used[i] = true;
    }
    let labels = chain_labels(members.iter().map(|&i| &chunks[i]).collect());
    let text = labels.join(" ");
    let mut inlines = Vec::new();
    for (n, label) in labels.iter().enumerate() {
        if n > 0 {
            inlines.push(Inline::LineBreak);
        }
        inlines.push(Inline::plain(label.clone()));
    }
    let region = members.iter().fold(region, |r, &i| {
        let c = &chunks[i];
        r.union(&Rect { x0: c.x0, y0: c.bottom(), x1: c.x1, y1: c.top() })
    });
    let (at_top, at_bottom) = edges(geometry, chunks, used, &region, body_size);
    Some(Found {
        page,
        region,
        blocks: vec![Block::Paragraph(inlines)],
        table: None,
        text,
        is_figure: true,
        at_top,
        at_bottom,
        has_caption: false,
    })
}

/// A figure's labels as lines of text. Text set along a curve arrives as
/// fragments; each fragment continues the one whose end it starts at.
fn chain_labels(mut pieces: Vec<&Chunk>) -> Vec<String> {
    pieces.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x0.total_cmp(&b.x0)));
    let n = pieces.len();
    let mut next: Vec<Option<usize>> = vec![None; n];
    let mut has_prev = vec![false; n];
    // Closest continuations first, so a near fragment is never taken by a
    // farther one.
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (i, a) in pieces.iter().enumerate() {
        let reach = a.size * 1.1;
        for (j, b) in pieces.iter().enumerate() {
            // Pieces on one baseline were joined already where they touch;
            // only text off the horizontal continues across baselines.
            if i == j
                || (b.size - a.size).abs() >= 0.5
                || b.x0 < a.x1 - reach
                || (b.y - a.y).abs() <= 1.5
            {
                continue;
            }
            let d = (b.x0 - a.x1).hypot(b.y - a.y);
            if d <= reach {
                pairs.push((d, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, i, j) in pairs {
        if next[i].is_none() && !has_prev[j] && !reaches(&next, j, i) {
            next[i] = Some(j);
            has_prev[j] = true;
        }
    }
    let mut labels: Vec<(f32, f32, String)> = Vec::new();
    let mut seen = vec![false; n];
    for start in 0..n {
        if has_prev[start] || seen[start] {
            continue;
        }
        let mut text = String::new();
        let mut at = Some(start);
        while let Some(i) = at {
            if seen[i] {
                break;
            }
            seen[i] = true;
            text.push_str(&pieces[i].text());
            at = next[i];
        }
        labels.push((
            pieces[start].y,
            pieces[start].x0,
            text.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
    }
    // Labels on one line read left to right.
    labels.sort_by(|a, b| {
        if (a.0 - b.0).abs() <= 2.0 { a.1.total_cmp(&b.1) } else { b.0.total_cmp(&a.0) }
    });
    let mut lines: Vec<(f32, String)> = Vec::new();
    for (y, _, text) in labels {
        if text.is_empty() {
            continue;
        }
        match lines.last_mut() {
            Some((ly, line)) if (*ly - y).abs() <= 2.0 => {
                line.push(' ');
                line.push_str(&text);
            }
            _ => lines.push((y, text)),
        }
    }
    lines.into_iter().map(|(_, t)| t).collect()
}

/// Whether following `next` from `from` arrives at `to` (joining them would
/// close a loop).
fn reaches(next: &[Option<usize>], from: usize, to: usize) -> bool {
    let mut at = Some(from);
    let mut steps = 0;
    while let Some(i) = at {
        if i == to {
            return true;
        }
        steps += 1;
        if steps > next.len() {
            return true;
        }
        at = next[i];
    }
    false
}

/// Whether anything but running heads lies above `region`, and anything
/// but notes and folios below it.
fn edges(
    geometry: &PageGeometry,
    chunks: &[Chunk],
    used: &[bool],
    region: &Rect,
    body_size: f32,
) -> (bool, bool) {
    let head = geometry.height * 0.92;
    let foot = geometry.height * 0.07;
    let above =
        chunks.iter().zip(used).any(|(c, u)| !u && c.bottom() > region.y1 && c.top() < head);
    let below = chunks
        .iter()
        .zip(used)
        .any(|(c, u)| !u && c.top() < region.y0 && c.bottom() > foot && c.size >= body_size * 0.92);
    (!above, !below)
}

// ---------------------------------------------------------------------------
// Tables

/// A cluster of rules and shading bands that may frame a table.
#[derive(Debug)]
struct Candidate {
    rect: Rect,
    hrules: Vec<Rule>,
    vrules: Vec<Rule>,
    bands: Vec<Rect>,
}

fn table_regions(geometry: &PageGeometry, chunks: &[Chunk], used: &[bool]) -> Vec<Candidate> {
    // Rules in the top and bottom margins underline running heads and set
    // off folios; they frame no table.
    let (head, foot) = (geometry.height * 0.93, geometry.height * 0.05);
    let hrules: Vec<Rule> = geometry
        .hrules
        .iter()
        .filter(|r| r.len() >= MIN_HRULE && r.at < head && r.at > foot)
        .copied()
        .collect();
    let vrules: Vec<Rule> = geometry
        .vrules
        .iter()
        .filter(|r| r.len() >= MIN_VRULE && r.from < head && r.to > foot)
        .copied()
        .collect();
    let bands: Vec<Rect> = geometry.fills.iter().filter(|f| is_band(f)).map(|f| f.rect).collect();

    // Nodes: 0..h are hrules, then vrules, then bands.
    let (nh, nv) = (hrules.len(), vrules.len());
    let total = nh + nv + bands.len();
    let rect_of = |i: usize| -> Rect {
        if i < nh {
            let r = hrules[i];
            Rect { x0: r.from, y0: r.at, x1: r.to, y1: r.at }
        } else if i < nh + nv {
            let r = vrules[i - nh];
            Rect { x0: r.at, y0: r.from, x1: r.at, y1: r.to }
        } else {
            bands[i - nh - nv]
        }
    };
    let mut parent: Vec<usize> = (0..total).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    // Only long runs of words can be prose; the rest never breaks a table.
    let wordy: Vec<&Chunk> = chunks
        .iter()
        .zip(used)
        .filter(|(c, u)| !**u && c.text().split_whitespace().count() >= 8)
        .map(|(c, _)| c)
        .collect();
    let prose_between = |a: &Rule, b: &Rule| -> bool {
        let (lo, hi) = if a.at < b.at { (a.at, b.at) } else { (b.at, a.at) };
        let (x0, x1) = (a.from.max(b.from), a.to.min(b.to));
        wordy.iter().any(|c| {
            c.y > lo && c.y < hi && c.x0 < x1 && c.x1 > x0 && c.x1 - c.x0 > (x1 - x0) * 0.6
        })
    };
    for i in 0..total {
        for j in i + 1..total {
            let (a, b) = (rect_of(i), rect_of(j));
            let connected = match (i < nh, j < nh, i < nh + nv, j < nh + nv) {
                // Two horizontal rules frame a table when they span about
                // the same width with no prose between them.
                (true, true, _, _) => {
                    let (ra, rb) = (hrules[i], hrules[j]);
                    let overlap = ra.to.min(rb.to) - ra.from.max(rb.from);
                    // A short rule (a group rule, a note separator) joins
                    // only a rule close by.
                    let comparable = ra.len().min(rb.len()) >= ra.len().max(rb.len()) * 0.5
                        || (ra.at - rb.at).abs() <= 60.0;
                    comparable
                        && overlap >= ra.len().min(rb.len()) * 0.7
                        && !prose_between(&ra, &rb)
                }
                // Vertical rules at one x continue each other across the
                // short gaps section titles leave.
                (false, false, true, true) => {
                    let (ra, rb) = (vrules[i - nh], vrules[j - nh]);
                    (ra.at - rb.at).abs() <= 2.0 && (ra.from - rb.to).max(rb.from - ra.to) <= 45.0
                }
                _ => a.expand(3.0).intersects(&b),
            };
            if connected {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Candidate> = Default::default();
    for i in 0..total {
        let r = root(&mut parent, i);
        let rect = rect_of(i);
        let entry = groups.entry(r).or_insert(Candidate {
            rect,
            hrules: vec![],
            vrules: vec![],
            bands: vec![],
        });
        entry.rect = entry.rect.union(&rect);
        if i < nh {
            entry.hrules.push(hrules[i]);
        } else if i < nh + nv {
            entry.vrules.push(vrules[i - nh]);
        } else {
            entry.bands.push(bands[i - nh - nv]);
        }
    }
    groups
        .into_values()
        .flat_map(split_unruled)
        .filter(|c| c.hrules.len() >= 2 || c.vrules.iter().map(Rule::len).sum::<f32>() >= 40.0)
        .collect()
}

/// Separate stacked grids: in a candidate with column rules, a band between
/// two horizontal rules that no column rule crosses divides two tables
/// (and holds whatever labels the second).
fn split_unruled(candidate: Candidate) -> Vec<Candidate> {
    if candidate.vrules.is_empty() || candidate.hrules.len() < 3 {
        return vec![candidate];
    }
    // Only rules across the whole candidate divide it; shorter ones
    // underline titles or group columns.
    let width = candidate.rect.width();
    let mut ys: Vec<f32> =
        candidate.hrules.iter().filter(|r| r.len() >= width * 0.85).map(|r| r.at).collect();
    ys.sort_by(|a, b| b.total_cmp(a));
    ys.dedup_by(|a, b| (*a - *b).abs() <= 1.0);
    let cuts: Vec<(f32, f32)> = ys
        .windows(2)
        .filter(|w| {
            let (hi, lo) = (w[0], w[1]);
            !candidate.vrules.iter().any(|v| v.to.min(hi) - v.from.max(lo) >= (hi - lo) * 0.5)
        })
        .map(|w| (w[0], w[1]))
        .collect();
    if cuts.is_empty() {
        return vec![candidate];
    }
    // Pieces between cuts: each takes the rules and bands inside it.
    let mut edges: Vec<f32> = vec![f32::INFINITY];
    for (hi, lo) in &cuts {
        edges.push((hi + lo) / 2.0);
    }
    edges.push(f32::NEG_INFINITY);
    edges
        .windows(2)
        .filter_map(|w| {
            let (hi, lo) = (w[0], w[1]);
            let hrules: Vec<Rule> =
                candidate.hrules.iter().filter(|r| r.at < hi && r.at > lo).copied().collect();
            let vrules: Vec<Rule> = candidate
                .vrules
                .iter()
                .filter(|r| (r.from + r.to) / 2.0 < hi && (r.from + r.to) / 2.0 > lo)
                .copied()
                .collect();
            let bands: Vec<Rect> = candidate
                .bands
                .iter()
                .filter(|b| (b.y0 + b.y1) / 2.0 < hi && (b.y0 + b.y1) / 2.0 > lo)
                .copied()
                .collect();
            let mut rect: Option<Rect> = None;
            for r in &hrules {
                let piece = Rect { x0: r.from, y0: r.at, x1: r.to, y1: r.at };
                rect = Some(rect.map_or(piece, |x| x.union(&piece)));
            }
            for r in &vrules {
                let piece = Rect { x0: r.at, y0: r.from, x1: r.at, y1: r.to };
                rect = Some(rect.map_or(piece, |x| x.union(&piece)));
            }
            Some(Candidate { rect: rect?, hrules, vrules, bands })
        })
        .collect()
}

/// A line of chunks sharing a baseline (loosely), for row building.
#[derive(Debug, Clone)]
struct Line {
    y: f32,
    size: f32,
    chunks: Vec<usize>,
}

fn lines_of(chunks: &[Chunk], members: &[usize]) -> Vec<Line> {
    let mut sorted = members.to_vec();
    sorted.sort_by(|&a, &b| chunks[b].y.total_cmp(&chunks[a].y));
    let mut lines: Vec<Line> = Vec::new();
    for i in sorted {
        let c = &chunks[i];
        match lines.last_mut() {
            Some(line) if (line.y - c.y).abs() <= line.size.min(c.size) * 0.3 => {
                line.chunks.push(i)
            }
            _ => lines.push(Line { y: c.y, size: c.size, chunks: vec![i] }),
        }
    }
    for line in &mut lines {
        line.chunks.sort_by(|&a, &b| chunks[a].x0.total_cmp(&chunks[b].x0));
    }
    lines
}

fn paragraph_of(chunks: &[Chunk], members: &[usize]) -> Vec<Inline> {
    let mut inlines: Vec<Inline> = Vec::new();
    for (n, &i) in members.iter().enumerate() {
        let spaced = inlines.last().is_none_or(
            |l| matches!(l, Inline::Text { text, .. } if text.ends_with(char::is_whitespace)),
        );
        if n > 0 && !spaced && !chunks[i].text().starts_with(char::is_whitespace) {
            // A space between two runs of one style takes that style, so the
            // runs stay one.
            let prev = match inlines.last() {
                Some(Inline::Text { style, .. }) => Some(*style),
                _ => None,
            };
            let next = chunks[i].runs.first().map(|(_, style)| *style);
            let style = if prev.is_some() && prev == next {
                prev.unwrap_or(Style::PLAIN)
            } else {
                Style::PLAIN
            };
            inlines.push(Inline::Text { text: " ".into(), style });
            if let [.., Inline::Text { text: a, style: sa }, Inline::Text { text: b, style: sb }] =
                &mut inlines[..]
                && sa == sb
            {
                a.push_str(b);
                inlines.pop();
            }
        }
        for (text, style) in &chunks[i].runs {
            push_text(&mut inlines, text, *style);
        }
    }
    trim_inlines(inlines)
}

fn push_text(inlines: &mut Vec<Inline>, text: &str, style: Style) {
    let style = if text.trim().is_empty() {
        match inlines.last() {
            Some(Inline::Text { style: prev, .. }) if *prev == style => style,
            _ => Style::PLAIN,
        }
    } else {
        style
    };
    if let Some(Inline::Text { text: prev, style: prev_style }) = inlines.last_mut()
        && *prev_style == style
    {
        prev.push_str(text);
        return;
    }
    inlines.push(Inline::Text { text: text.to_string(), style });
}

fn trim_inlines(mut inlines: Vec<Inline>) -> Vec<Inline> {
    for inline in &mut inlines {
        if let Inline::Text { text, .. } = inline {
            let starts = text.starts_with(char::is_whitespace);
            let ends = text.ends_with(char::is_whitespace);
            let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
            *text = if words.is_empty() {
                " ".to_string()
            } else {
                format!("{}{words}{}", if starts { " " } else { "" }, if ends { " " } else { "" })
            };
        }
    }
    // Rejoin: collapse the spaces the normalization above left at run edges.
    let mut out: Vec<Inline> = Vec::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => push_text(&mut out, &text, style),
            other => out.push(other),
        }
    }
    if let Some(Inline::Text { text, .. }) = out.first_mut() {
        *text = text.trim_start().to_string();
    }
    if let Some(Inline::Text { text, .. }) = out.last_mut() {
        *text = text.trim_end().to_string();
    }
    out.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
    out
}

fn table(
    page: u32,
    geometry: &PageGeometry,
    chunks: &[Chunk],
    used: &mut [bool],
    candidate: Candidate,
    body_size: f32,
) -> Option<Found> {
    let mut rect = candidate.rect;
    let interior_vrules: Vec<f32> = {
        let mut xs: Vec<f32> = candidate
            .vrules
            .iter()
            .map(|r| r.at)
            .filter(|&x| candidate.hrules.is_empty() || (x > rect.x0 + 5.0 && x < rect.x1 - 5.0))
            .collect();
        xs.sort_by(f32::total_cmp);
        xs.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
        xs
    };
    // A table split by vertical rules alone takes its width from its text.
    if candidate.hrules.is_empty() || rect.width() < 20.0 {
        let span: Vec<&Chunk> = chunks
            .iter()
            .zip(used.iter())
            .filter(|(c, u)| !**u && c.y >= rect.y0 - 2.0 && c.y <= rect.y1 + 2.0)
            .map(|(c, _)| c)
            .collect();
        let x0 = span.iter().map(|c| c.x0).fold(f32::INFINITY, f32::min);
        let x1 = span.iter().map(|c| c.x1).fold(f32::NEG_INFINITY, f32::max);
        if !x0.is_finite() || !x1.is_finite() {
            return None;
        }
        rect = Rect { x0: x0.min(rect.x0), y0: rect.y0, x1: x1.max(rect.x1), y1: rect.y1 };
    }
    let inside = |c: &Chunk| {
        c.center_x() >= rect.x0 - 3.0
            && c.center_x() <= rect.x1 + 3.0
            && c.y >= rect.y0 - 1.0
            && c.bottom() <= rect.y1 + 1.0
    };
    let mut members: Vec<usize> =
        (0..chunks.len()).filter(|&i| !used[i] && inside(&chunks[i])).collect();
    if members.len() < 2 {
        return None;
    }

    // A shading band across the top holds the table's title.
    let mut title: Vec<usize> = Vec::new();
    let mut top = rect.y1;
    if let Some(band) = candidate
        .bands
        .iter()
        .filter(|b| b.y1 >= rect.y1 - 3.0 && b.width() >= rect.width() * 0.6)
        .max_by(|a, b| a.y1.total_cmp(&b.y1))
    {
        let in_band: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| band.contains_point(chunks[i].center_x(), chunks[i].center_y()))
            .collect();
        let columns_in_band = lines_of(chunks, &in_band).iter().all(|l| l.chunks.len() == 1);
        if !in_band.is_empty() && columns_in_band {
            title = in_band;
            members.retain(|i| !title.contains(i));
            top = band.y0;
        }
    }

    let full = |r: &Rule| r.len() >= rect.width() * 0.85;
    // Without a band, a title may sit alone between the top two rules.
    if title.is_empty() {
        let below_top = candidate
            .hrules
            .iter()
            .filter(|r| full(r) && r.at < rect.y1 - 3.0 && r.at > rect.y0 + 3.0)
            .map(|r| r.at)
            .fold(f32::NEG_INFINITY, f32::max);
        if below_top.is_finite() {
            let above: Vec<usize> =
                members.iter().copied().filter(|&i| chunks[i].center_y() > below_top).collect();
            let centered =
                |c: &Chunk| (c.center_x() - (rect.x0 + rect.x1) / 2.0).abs() < rect.width() * 0.15;
            if let [only] = above[..]
                && centered(&chunks[only])
            {
                title = vec![only];
                members.retain(|&i| i != only);
                top = below_top;
            }
        }
    }
    // Full-width rules strictly inside the table, below the title.
    let mut inner: Vec<Rule> = candidate
        .hrules
        .iter()
        .filter(|r| full(r) && r.at < top - 3.0 && r.at > rect.y0 + 3.0)
        .copied()
        .collect();
    inner.sort_by(|a, b| b.at.total_cmp(&a.at));

    let built = if !interior_vrules.is_empty() {
        // A grid rules every row: each band between full-width rules holds
        // a line or three.
        let mut ys: Vec<f32> = candidate
            .hrules
            .iter()
            .filter(|r| full(r) && r.at <= top + 1.0)
            .map(|r| r.at)
            .collect();
        ys.sort_by(|a, b| b.total_cmp(a));
        ys.dedup_by(|a, b| (*a - *b).abs() <= 1.0);
        let grid_like = ys.len() >= 2
            && ys.windows(2).all(|w| {
                let band: Vec<usize> = members
                    .iter()
                    .copied()
                    .filter(|&i| chunks[i].center_y() < w[0] && chunks[i].center_y() > w[1])
                    .collect();
                lines_of(chunks, &band).len() <= 3
            });
        if grid_like {
            grid_table(chunks, &members, &candidate, &interior_vrules, rect, top)
        } else {
            split_table(chunks, &members, &candidate, &interior_vrules, rect, &inner)
        }
    } else {
        aligned_table(chunks, &members, &candidate, rect, top, &inner)
    }?;
    let (table, placed) = built;
    if !valid(&table) {
        return None;
    }
    for &i in placed.iter().chain(&title) {
        used[i] = true;
    }

    // Captions just above ("Tavola 1.2"), notes just below.
    let caption = caption_above(chunks, used, &rect, body_size);
    for &i in &caption {
        used[i] = true;
    }
    let notes = notes_below(chunks, used, &rect, body_size);
    for &i in &notes {
        used[i] = true;
    }

    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut add_text = |ids: &[usize]| {
        for &i in ids {
            text.push_str(&chunks[i].text());
            text.push(' ');
        }
    };
    add_text(&caption);
    add_text(&title);
    add_text(&placed);
    add_text(&notes);
    for line in lines_of(chunks, &caption) {
        blocks.push(Block::Paragraph(paragraph_of(chunks, &line.chunks)));
    }
    if !title.is_empty() {
        let mut ordered = title.clone();
        ordered.sort_by(|&a, &b| {
            chunks[b].y.total_cmp(&chunks[a].y).then(chunks[a].x0.total_cmp(&chunks[b].x0))
        });
        blocks.push(Block::Paragraph(paragraph_of(chunks, &ordered)));
    }
    let table_index = blocks.len();
    blocks.push(Block::Table(table));
    if !notes.is_empty() {
        let mut ordered = notes.clone();
        ordered.sort_by(|&a, &b| {
            chunks[b].y.total_cmp(&chunks[a].y).then(chunks[a].x0.total_cmp(&chunks[b].x0))
        });
        blocks.push(Block::Paragraph(paragraph_of(chunks, &ordered)));
    }
    let (at_top, at_bottom) = edges(geometry, chunks, used, &rect, body_size);
    Some(Found {
        page,
        region: rect,
        blocks,
        table: Some(table_index),
        text,
        is_figure: false,
        at_top,
        at_bottom,
        has_caption: !caption.is_empty() || !title.is_empty(),
    })
}

/// At least two columns and two rows with something in them.
fn valid(table: &Table) -> bool {
    use crate::model::CellSlot;
    let filled_rows = table
        .grid
        .iter()
        .filter(|row| row.iter().any(|s| matches!(s, CellSlot::Origin(c) if !c.is_empty())))
        .count();
    let width = table.grid.iter().map(Vec::len).max().unwrap_or(0);
    let multi = table
        .grid
        .iter()
        .filter(|row| {
            row.iter().filter(|s| matches!(s, CellSlot::Origin(c) if !c.is_empty())).count() >= 2
        })
        .count();
    // A one-row box of two or more cells is a table too.
    (filled_rows >= 2 || (filled_rows == 1 && multi == 1 && table.grid.len() == 1))
        && width >= 2
        && multi >= 1
}

fn caption_above(chunks: &[Chunk], used: &[bool], rect: &Rect, body_size: f32) -> Vec<usize> {
    let mut picked: Vec<usize> = Vec::new();
    let mut edge = rect.y1;
    loop {
        let next: Vec<usize> = (0..chunks.len())
            .filter(|&i| {
                let c = &chunks[i];
                !used[i]
                    && !picked.contains(&i)
                    && c.bottom() >= edge - 1.0
                    && c.bottom() - edge <= body_size * 1.6
                    && c.x0 >= rect.x0 - 5.0
                    && c.x1 <= rect.x1 + 5.0
                    && c.x1 - c.x0 < rect.width() * 0.45
            })
            .collect();
        if next.is_empty() || picked.len() > 4 {
            break;
        }
        edge = next.iter().map(|&i| chunks[i].top()).fold(edge, f32::max);
        picked.extend(next);
    }
    // Only a line that stands alone is a caption, not the end of a paragraph.
    let lone = picked.iter().all(|&i| {
        !chunks
            .iter()
            .zip(used)
            .enumerate()
            .any(|(j, (c, u))| !u && !picked.contains(&j) && (c.y - chunks[i].y).abs() < 2.0)
    });
    if lone { picked } else { Vec::new() }
}

fn notes_below(chunks: &[Chunk], used: &[bool], rect: &Rect, body_size: f32) -> Vec<usize> {
    let mut picked: Vec<usize> = Vec::new();
    let mut edge = rect.y0;
    loop {
        let next: Vec<usize> = (0..chunks.len())
            .filter(|&i| {
                let c = &chunks[i];
                !used[i]
                    && !picked.contains(&i)
                    && c.top() <= edge + 1.0
                    && edge - c.top() <= c.size * 1.2
                    && c.size < body_size * 0.93
                    && c.x0 >= rect.x0 - 5.0
                    && c.x1 <= rect.x1 + 5.0
            })
            .collect();
        if next.is_empty() {
            break;
        }
        edge = next.iter().map(|&i| chunks[i].bottom()).fold(edge, f32::min);
        picked.extend(next);
    }
    picked
}

type Built = (Table, Vec<usize>);

/// A header label: its chunks and the last column it spans.
type Label = (Vec<usize>, usize);

/// Split `columns` (in order) into one contiguous run per label (centers in
/// order), each label as centered over its run as the split allows.
fn partition(labels: &[f32], columns: &[(f32, f32)]) -> Vec<(usize, usize)> {
    let (n, m) = (labels.len(), columns.len());
    if n == 0 || m < n {
        return Vec::new();
    }
    let cost = |label: usize, s: usize, e: usize| {
        (labels[label] - (columns[s].0 + columns[e].1) / 2.0).abs()
    };
    // best[i][j]: first i labels over the first j columns.
    let mut best = vec![vec![f32::INFINITY; m + 1]; n + 1];
    let mut from = vec![vec![0usize; m + 1]; n + 1];
    best[0][0] = 0.0;
    for i in 1..=n {
        for j in i..=m {
            for s in (i - 1)..j {
                let total = best[i - 1][s] + cost(i - 1, s, j - 1);
                if total < best[i][j] {
                    best[i][j] = total;
                    from[i][j] = s;
                }
            }
        }
    }
    let mut spans = vec![(0, 0); n];
    let mut j = m;
    for i in (1..=n).rev() {
        let s = from[i][j];
        spans[i - 1] = (s, j - 1);
        j = s;
    }
    spans
}

/// Columns bounded by vertical rules, rows by horizontal ones.
fn grid_table(
    chunks: &[Chunk],
    members: &[usize],
    candidate: &Candidate,
    xs: &[f32],
    rect: Rect,
    top: f32,
) -> Option<Built> {
    let mut ys: Vec<f32> = candidate
        .hrules
        .iter()
        .filter(|r| r.len() >= rect.width() * 0.5 && r.at <= top + 1.0)
        .map(|r| r.at)
        .collect();
    ys.push(top.min(rect.y1));
    ys.push(rect.y0);
    ys.sort_by(|a, b| b.total_cmp(a));
    ys.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
    let mut bounds = vec![rect.x0];
    bounds.extend(xs.iter().copied());
    bounds.push(rect.x1);
    let cols = bounds.len() - 1;
    let mut builder = GridBuilder::new();
    let mut placed = Vec::new();
    let mut header_bold = None;
    let mut body_plain = false;
    for (r, band) in ys.windows(2).enumerate() {
        let (hi, lo) = (band[0], band[1]);
        let in_band: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| chunks[i].center_y() < hi && chunks[i].center_y() > lo)
            .collect();
        if in_band.is_empty() {
            continue;
        }
        builder.next_row();
        // A boundary is present in this band when a rule covers most of it.
        let present: Vec<bool> = xs
            .iter()
            .map(|&x| {
                candidate.vrules.iter().any(|v| {
                    (v.at - x).abs() <= 2.0 && v.to.min(hi) - v.from.max(lo) >= (hi - lo) * 0.6
                })
            })
            .collect();
        let mut c = 0;
        while c < cols {
            let mut end = c + 1;
            while end < cols && !present[end - 1] {
                end += 1;
            }
            let (x0, x1) = (bounds[c], bounds[end]);
            let mut cell: Vec<usize> = in_band
                .iter()
                .copied()
                .filter(|&i| chunks[i].center_x() >= x0 && chunks[i].center_x() < x1)
                .collect();
            cell.sort_by(|&a, &b| {
                chunks[b].y.total_cmp(&chunks[a].y).then(chunks[a].x0.total_cmp(&chunks[b].x0))
            });
            if r == 0 && !cell.is_empty() {
                header_bold =
                    Some(header_bold.unwrap_or(true) && cell.iter().all(|&i| chunks[i].bold()));
            } else if cell.iter().any(|&i| !chunks[i].bold()) {
                body_plain = true;
            }
            placed.extend(cell.iter().copied());
            let blocks = block_of(chunks, &cell);
            builder.place(Cell::spanning(blocks, (end - c) as u32, 1)).ok()?;
            c = end;
        }
    }
    let mut table = builder.finish(TableKind::Data);
    let declared = usize::from(header_bold == Some(true) && body_plain);
    table.header_rows = resolve_header_rows(&table, declared);
    Some((table, placed))
}

fn block_of(chunks: &[Chunk], ids: &[usize]) -> Vec<Block> {
    if ids.is_empty() {
        return Vec::new();
    }
    let inlines = paragraph_of(chunks, ids);
    if inlines.is_empty() { Vec::new() } else { vec![Block::Paragraph(inlines)] }
}

/// Columns split by vertical rules, entries in each column independent of
/// the other, section titles across the gaps in the rules.
fn split_table(
    chunks: &[Chunk],
    members: &[usize],
    candidate: &Candidate,
    xs: &[f32],
    rect: Rect,
    inner: &[Rule],
) -> Option<Built> {
    let mut bounds = vec![rect.x0 - 1.0];
    bounds.extend(xs.iter().copied());
    bounds.push(rect.x1 + 1.0);
    let cols = bounds.len() - 1;
    let covered = |x: f32, y: f32| {
        candidate
            .vrules
            .iter()
            .any(|v| (v.at - x).abs() <= 2.0 && y >= v.from - 2.0 && y <= v.to + 2.0)
    };
    // A header sits above the first full-width rule, when one is near the top.
    let header_rule = inner.first().filter(|r| {
        members.iter().filter(|&&i| chunks[i].y > r.at).count() <= cols * 3
            && members.iter().any(|&i| chunks[i].y < r.at)
    });
    let mut header: Vec<usize> = Vec::new();
    let mut body: Vec<usize> = Vec::new();
    for &i in members {
        match header_rule {
            Some(r) if chunks[i].y > r.at => header.push(i),
            _ => body.push(i),
        }
    }
    // Section titles: text crossing a boundary where no rule runs.
    let mut sections: Vec<usize> = Vec::new();
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); cols];
    for &i in &body {
        let c = &chunks[i];
        let crosses =
            xs.iter().any(|&x| c.x0 < x - 2.0 && c.x1 > x + 2.0 && !covered(x, c.center_y()));
        let centered_in_gap = xs.iter().all(|&x| !covered(x, c.center_y()));
        if crosses || (centered_in_gap && !xs.is_empty()) {
            sections.push(i);
        } else {
            let col =
                (0..cols).find(|&k| c.center_x() >= bounds[k] && c.center_x() < bounds[k + 1])?;
            columns[col].push(i);
        }
    }
    // Entries: runs of lines in one column with ordinary line spacing.
    let mut entries: Vec<(usize, f32, f32, Vec<usize>)> = Vec::new(); // (col, top, bottom, chunks)
    for (col, ids) in columns.iter().enumerate() {
        let lines = lines_of(chunks, ids);
        if lines.is_empty() {
            continue;
        }
        let gaps: Vec<f32> = lines.windows(2).map(|w| w[0].y - w[1].y).collect();
        // Ordinary line spacing: the tightest gap in the column, unless
        // every entry is one line long, when no gap is ordinary.
        let pitch = {
            let mut sorted = gaps.clone();
            sorted.sort_by(f32::total_cmp);
            let size = lines[0].size;
            sorted.first().copied().unwrap_or(size * 1.2).max(size).min(size * 1.5)
        };
        let mut current: Vec<usize> = Vec::new();
        let mut last_y = f32::NAN;
        for line in &lines {
            if !current.is_empty() && (last_y - line.y) > pitch * 1.25 {
                entries.push(entry(chunks, col, std::mem::take(&mut current)));
            }
            current.extend(line.chunks.iter().copied());
            last_y = line.y;
        }
        if !current.is_empty() {
            entries.push(entry(chunks, col, current));
        }
    }
    // Rows: entries that overlap vertically sit side by side.
    entries.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut rows: Vec<(f32, f32, Vec<Vec<usize>>)> = Vec::new(); // (top, bottom, per column)
    for (col, top, bottom, ids) in entries {
        let overlapping = rows.iter_mut().rev().find(|(t, b, _)| top > *b && bottom < *t);
        match overlapping {
            Some((t, b, cells)) => {
                *t = t.max(top);
                *b = b.min(bottom);
                if !cells[col].is_empty() {
                    cells[col].push(usize::MAX);
                }
                cells[col].extend(ids);
            }
            None => {
                let mut cells = vec![Vec::new(); cols];
                cells[col] = ids;
                rows.push((top, bottom, cells));
            }
        }
    }
    // Section titles become full-width rows in place.
    let mut section_lines = lines_of(chunks, &sections);
    section_lines.sort_by(|a, b| b.y.total_cmp(&a.y));
    enum Row {
        Cells(Vec<Vec<usize>>),
        Section(Vec<usize>),
    }
    let mut ordered: Vec<(f32, Row)> =
        rows.into_iter().map(|(t, _, c)| (t, Row::Cells(c))).collect();
    for line in section_lines {
        ordered.push((line.y + line.size, Row::Section(line.chunks)));
    }
    ordered.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut builder = GridBuilder::new();
    let mut placed = Vec::new();
    let mut header_rows = 0;
    if !header.is_empty() {
        builder.next_row();
        for col in 0..cols {
            let ids: Vec<usize> = header
                .iter()
                .copied()
                .filter(|&i| {
                    chunks[i].center_x() >= bounds[col] && chunks[i].center_x() < bounds[col + 1]
                })
                .collect();
            placed.extend(ids.iter().copied());
            builder.place(Cell::new(block_of(chunks, &ids))).ok()?;
        }
        header_rows = 1;
    }
    for (_, row) in ordered {
        builder.next_row();
        match row {
            Row::Section(ids) => {
                placed.extend(ids.iter().copied());
                builder.place(Cell::spanning(block_of(chunks, &ids), cols as u32, 1)).ok()?;
            }
            Row::Cells(cells) => {
                for cell in cells {
                    let mut blocks = Vec::new();
                    for part in cell.split(|&i| i == usize::MAX) {
                        placed.extend(part.iter().copied());
                        blocks.extend(block_of(chunks, part));
                    }
                    builder.place(Cell::new(blocks)).ok()?;
                }
            }
        }
    }
    let mut table = builder.finish(TableKind::Data);
    table.header_rows = header_rows;
    Some((table, placed))
}

fn entry(chunks: &[Chunk], col: usize, mut ids: Vec<usize>) -> (usize, f32, f32, Vec<usize>) {
    ids.sort_by(|&a, &b| {
        chunks[b].y.total_cmp(&chunks[a].y).then(chunks[a].x0.total_cmp(&chunks[b].x0))
    });
    let top = ids.iter().map(|&i| chunks[i].top()).fold(f32::NEG_INFINITY, f32::max);
    let bottom = ids.iter().map(|&i| chunks[i].bottom()).fold(f32::INFINITY, f32::min);
    (col, top, bottom, ids)
}

/// Columns read from text alignment: a table ruled only horizontally, its
/// header grouped by short rules under the group labels.
fn aligned_table(
    chunks: &[Chunk],
    members: &[usize],
    candidate: &Candidate,
    rect: Rect,
    top: f32,
    inner: &[Rule],
) -> Option<Built> {
    // The header ends at the first full-width rule with rows below it.
    let header_rule = inner.iter().find(|r| {
        let below = members.iter().filter(|&&i| chunks[i].y < r.at).count();
        let above = members.iter().filter(|&&i| chunks[i].y > r.at).count();
        below >= 2 && above <= 24
    });
    let (header, body): (Vec<usize>, Vec<usize>) = match header_rule {
        Some(r) => members.iter().partition(|&&i| chunks[i].y > r.at),
        None => (Vec::new(), members.to_vec()),
    };
    let body_lines = lines_of(chunks, &body);
    let multi: Vec<&Line> = body_lines.iter().filter(|l| l.chunks.len() >= 2).collect();
    if multi.len() < 2 || multi.len() * 2 < body_lines.len() {
        return None;
    }
    // Columns: the x ranges body cells occupy, separated by gaps no
    // multi-cell row fills.
    let mut spans: Vec<(f32, f32)> =
        multi.iter().flat_map(|l| l.chunks.iter().map(|&i| (chunks[i].x0, chunks[i].x1))).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut columns: Vec<(f32, f32)> = Vec::new();
    for (x0, x1) in spans {
        match columns.last_mut() {
            Some(last) if x0 <= last.1 + 3.0 => last.1 = last.1.max(x1),
            _ => columns.push((x0, x1)),
        }
    }
    let cols = columns.len();
    if cols < 2 {
        return None;
    }
    // Each column's cell extent: out to the middle of the gaps around it,
    // and to the table's edges.
    let extents: Vec<(f32, f32)> = (0..cols)
        .map(|k| {
            let left = if k == 0 {
                rect.x0.min(columns[0].0)
            } else {
                (columns[k - 1].1 + columns[k].0) / 2.0
            };
            let right = if k + 1 == cols {
                rect.x1.max(columns[k].1)
            } else {
                (columns[k].1 + columns[k + 1].0) / 2.0
            };
            (left, right)
        })
        .collect();
    let column_of = |c: &Chunk| -> Option<(usize, usize)> {
        let overlapping: Vec<usize> =
            (0..cols).filter(|&k| c.x0 < columns[k].1 - 1.0 && c.x1 > columns[k].0 + 1.0).collect();
        match (overlapping.first(), overlapping.last()) {
            (Some(&a), Some(&b)) => Some((a, b)),
            _ => {
                // In a gap: the nearest column by center.
                let k = (0..cols).min_by(|&a, &b| {
                    let da = (columns[a].0 + columns[a].1) / 2.0 - c.center_x();
                    let db = (columns[b].0 + columns[b].1) / 2.0 - c.center_x();
                    da.abs().total_cmp(&db.abs())
                })?;
                Some((k, k))
            }
        }
    };

    let mut builder = GridBuilder::new();
    let mut placed: Vec<usize> = Vec::new();

    // Header tiers, split at the short rules that group columns.
    let mut group_rules: Vec<Rule> = candidate
        .hrules
        .iter()
        .filter(|r| {
            header_rule.is_some_and(|h| r.at > h.at + 1.0)
                && r.at < top - 1.0
                && r.len() < rect.width() * 0.85
        })
        .copied()
        .collect();
    group_rules.sort_by(|a, b| b.at.total_cmp(&a.at));
    let mut levels: Vec<f32> = group_rules.iter().map(|r| r.at).collect();
    levels.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
    let tiers = levels.len() + 1;
    let mut header_rows = 0;
    if !header.is_empty() {
        // tier -> column -> (chunks, span end)
        let mut grid: Vec<Vec<Option<Label>>> = vec![vec![None; cols]; tiers];
        let mut sorted = header.clone();
        sorted.sort_by(|&a, &b| {
            chunks[b].y.total_cmp(&chunks[a].y).then(chunks[a].x0.total_cmp(&chunks[b].x0))
        });
        let tier_of = |c: &Chunk| levels.iter().filter(|&&y| c.center_y() < y).count();
        // The group rule right under a label, which sets its span.
        let rule_under = |c: &Chunk| {
            group_rules.iter().position(|r| {
                r.at < c.bottom() + 1.0
                    && c.bottom() - r.at <= c.size * 1.5
                    && c.center_x() >= r.from - 2.0
                    && c.center_x() <= r.to + 2.0
            })
        };
        for &i in &sorted {
            let c = &chunks[i];
            let tier = tier_of(c);
            let (first, last) = match rule_under(c) {
                Some(r) => {
                    let rule = group_rules[r];
                    // Labels sharing one rule (drawn unbroken under several
                    // groups) split its columns so each sits centered over
                    // its own.
                    let mut peers: Vec<f32> = header
                        .iter()
                        .map(|&j| &chunks[j])
                        .filter(|p| tier_of(p) == tier && rule_under(p) == Some(r))
                        .map(Chunk::center_x)
                        .collect();
                    peers.sort_by(f32::total_cmp);
                    peers.dedup_by(|a, b| (*a - *b).abs() < 0.5);
                    let under: Vec<usize> = (0..cols)
                        .filter(|&k| {
                            let (a, b) = columns[k];
                            b.min(rule.to) - a.max(rule.from) >= (b - a) * 0.5
                        })
                        .collect();
                    let spans =
                        partition(&peers, &under.iter().map(|&k| extents[k]).collect::<Vec<_>>());
                    let me = peers.iter().position(|p| (p - c.center_x()).abs() < 0.5);
                    match (me.and_then(|m| spans.get(m)), under.is_empty()) {
                        (Some(&(a, b)), false) => (under[a], under[b]),
                        _ => column_of(c)?,
                    }
                }
                None => column_of(c)?,
            };
            let slot = &mut grid[tier][first];
            match slot {
                Some((ids, end)) => {
                    ids.push(i);
                    *end = (*end).max(last);
                }
                None => *slot = Some((vec![i], last)),
            }
        }
        // The stub head (over the row labels) often straddles the group
        // rule; all of it is one cell.
        let stub_spanned = (0..tiers).any(|t| grid[t][0].as_ref().is_some_and(|(_, end)| *end > 0));
        if tiers > 1 && !stub_spanned {
            let mut stub: Vec<usize> = Vec::new();
            for tier in grid.iter_mut() {
                if let Some((ids, _)) = tier[0].take() {
                    stub.extend(ids);
                }
            }
            if !stub.is_empty() {
                grid[tiers - 1][0] = Some((stub, 0));
            }
        }
        // A lone label in a tier of its own that names no single column
        // ("(valori assoluti)" under five years) covers all the data columns.
        for tier in grid.iter_mut().skip(1) {
            let labels: Vec<usize> = (1..cols).filter(|&k| tier[k].is_some()).collect();
            if let [only] = labels[..]
                && cols > 3
            {
                let cell = tier[only].take().map(|(ids, _)| (ids, cols - 1));
                tier[1] = cell;
            }
        }
        // A label alone in its column across the tiers (the stub head)
        // spans them all.
        let mut row_spans = vec![vec![1u32; cols]; tiers];
        for col in 0..cols {
            let filled: Vec<usize> = (0..tiers).filter(|&t| grid[t][col].is_some()).collect();
            // Only where no label of another tier spans over the column.
            let spanned = (0..tiers).any(|t| {
                (0..col).any(|start| grid[t][start].as_ref().is_some_and(|(_, end)| *end >= col))
            });
            if tiers > 1
                && !spanned
                && filled.len() == 1
                && grid[filled[0]][col].as_ref().is_some_and(|(_, end)| *end == col)
            {
                let t = filled[0];
                let cell = grid[t][col].take();
                grid[0][col] = cell;
                row_spans[0][col] = tiers as u32;
            }
        }
        for t in 0..tiers {
            builder.next_row();
            let mut col = 0;
            while col < cols {
                // Positions under a stub head spanning from the first tier.
                if t > 0
                    && (0..t).any(|u| row_spans[u][col] as usize > t - u && grid[u][col].is_some())
                {
                    col += 1;
                    continue;
                }
                match &grid[t][col] {
                    Some((ids, end)) => {
                        let mut ids = ids.clone();
                        ids.sort_by(|&a, &b| {
                            chunks[b]
                                .y
                                .total_cmp(&chunks[a].y)
                                .then(chunks[a].x0.total_cmp(&chunks[b].x0))
                        });
                        placed.extend(ids.iter().copied());
                        let span = (*end - col + 1) as u32;
                        builder
                            .place(Cell::spanning(block_of(chunks, &ids), span, row_spans[t][col]))
                            .ok()?;
                        col = end + 1;
                    }
                    None => {
                        builder.place(Cell::default()).ok()?;
                        col += 1;
                    }
                }
            }
        }
        header_rows = tiers;
    }

    // Body rows. A label wrapped over lines with nothing else on them
    // belongs to the row its values sit on: the next line, when the values
    // are set on the label's last line, else the one before.
    let mut rows: Vec<Vec<Vec<usize>>> = Vec::new();
    let mut row_y: Vec<f32> = Vec::new();
    let mut carry: Vec<usize> = Vec::new();
    let mut carry_y = f32::NAN;
    for (n, line) in body_lines.iter().enumerate() {
        let mut cells: Vec<Vec<usize>> = vec![Vec::new(); cols];
        let mut spans_many = false;
        for &i in &line.chunks {
            let (a, b) = column_of(&chunks[i])?;
            spans_many |= b > a;
            cells[a].push(i);
        }
        let label_only =
            cols > 1 && !spans_many && cells[1..].iter().all(Vec::is_empty) && !cells[0].is_empty();
        let pitch = line.size * 1.45;
        if label_only {
            let text: String = cells[0].iter().map(|&i| chunks[i].text()).collect();
            let continues = text.trim_start().starts_with(|c: char| c.is_lowercase());
            let prev_close = row_y.last().is_some_and(|&y| y - line.y <= pitch);
            // Lowercase text right under a row finishes that row's label.
            if continues
                && prev_close
                && carry.is_empty()
                && let Some(prev) = rows.last_mut()
            {
                prev[0].extend(cells[0].iter().copied());
                row_y.push(line.y);
                continue;
            }
            let next_close = body_lines.get(n + 1).is_some_and(|next| line.y - next.y <= pitch);
            if next_close || (!carry.is_empty() && carry_y - line.y <= pitch) {
                carry.extend(cells[0].iter().copied());
                carry_y = line.y;
                continue;
            }
        }
        if !carry.is_empty() {
            let mut first = std::mem::take(&mut carry);
            first.extend(cells[0].iter().copied());
            cells[0] = first;
        }
        rows.push(cells);
        row_y.push(line.y);
    }
    if !carry.is_empty() {
        let mut cells = vec![Vec::new(); cols];
        cells[0] = carry;
        rows.push(cells);
    }
    for cells in rows {
        builder.next_row();
        let mut col = 0;
        while col < cols {
            let ids = &cells[col];
            // A cell whose text runs over later columns spans them.
            let end = ids
                .iter()
                .filter_map(|&i| column_of(&chunks[i]).map(|(_, b)| b))
                .max()
                .unwrap_or(col)
                .max(col);
            let end =
                (col..=end).take_while(|&k| k == col || cells[k].is_empty()).last().unwrap_or(col);
            placed.extend(ids.iter().copied());
            builder.place(Cell::spanning(block_of(chunks, ids), (end - col + 1) as u32, 1)).ok()?;
            col = end + 1;
        }
    }
    let mut table = builder.finish(TableKind::Data);
    table.header_rows = resolve_header_rows(&table, header_rows);
    Some((table, placed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(x0: f32, x1: f32, y: f32, text: &str) -> Chunk {
        Chunk { x0, x1, y, size: 10.0, runs: vec![(text.to_string(), Style::PLAIN)] }
    }

    #[test]
    fn labels_sharing_a_rule_split_it_by_centering() {
        // "2017" over two columns and "2018" over three, one rule under both.
        let columns =
            [(174.0, 236.0), (236.0, 297.0), (297.0, 362.0), (362.0, 427.0), (427.0, 493.0)];
        assert_eq!(partition(&[251.0, 409.6], &columns), vec![(0, 1), (2, 4)]);
        assert_eq!(partition(&[330.0], &columns), vec![(0, 4)]);
    }

    #[test]
    fn text_set_along_a_curve_rejoins() {
        let pieces = [
            chunk(379.8, 443.0, 725.3, "Istituti di p"),
            chunk(442.8, 455.0, 718.8, "ag"),
            chunk(454.9, 460.5, 711.7, "a"),
            chunk(460.5, 469.1, 707.7, "m"),
            chunk(468.8, 473.8, 700.0, "e"),
            chunk(473.6, 481.0, 694.6, "nt"),
            chunk(480.9, 484.8, 684.4, "o"),
            chunk(203.4, 253.7, 726.3, "Banche"),
        ];
        let labels = chain_labels(pieces.iter().collect());
        assert_eq!(labels, vec!["Banche Istituti di pagamento"]);
    }

    #[test]
    fn touching_items_on_a_baseline_form_one_chunk() {
        let item = |x: f32, width: f32, text: &str| TextItem {
            text: text.into(),
            x,
            y: 549.1,
            width,
            height: 11.0,
            font: "F".into(),
            font_size: 11.0,
            page: 1,
            is_bold: true,
            is_italic: false,
            is_underline: false,
            is_strikeout: false,
            item_type: pdf_inspector::types::ItemType::Text,
            mcid: None,
        };
        let chunks =
            chunks(&[item(283.5, 14.7, "201"), item(298.1, 5.2, "7"), item(421.6, 19.8, "2018")]);
        let texts: Vec<String> = chunks.iter().map(Chunk::text).collect();
        assert_eq!(texts, vec!["2017", "2018"]);
    }
}
