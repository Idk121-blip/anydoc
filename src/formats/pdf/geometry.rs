//! What a page draws besides text: rules, filled areas and images.
//!
//! Tables and figures announce themselves in the drawing: ruled tables by
//! their horizontal and vertical rules, charts and infographics by filled
//! shapes and images with short labels over them. This walks a page's
//! content stream (and the form XObjects it draws) with the current
//! transformation matrix, and reduces every painted path to one of those
//! shapes in page space, the same space pdf-inspector reports text in.

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashSet;

/// Operators read per page before the rest is ignored.
const MAX_OPERATIONS: usize = 400_000;
/// Nesting of form XObjects followed.
const MAX_FORM_DEPTH: usize = 6;
/// A painted rectangle this thin (in points) is a rule, not an area.
const RULE_THICKNESS: f32 = 2.5;

/// An axis-aligned box in page space: `x0 < x1`, `y0 < y1`, y upwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn expand(&self, by: f32) -> Rect {
        Rect { x0: self.x0 - by, y0: self.y0 - by, x1: self.x1 + by, y1: self.y1 + by }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    pub fn contains_point(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// A horizontal (`at` is y, the span runs over x) or vertical (`at` is x,
/// the span runs over y) rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rule {
    pub at: f32,
    pub from: f32,
    pub to: f32,
}

impl Rule {
    pub fn len(&self) -> f32 {
        self.to - self.from
    }
}

/// A filled area that is not a rule.
#[derive(Debug, Clone, Copy)]
pub(super) struct Fill {
    pub rect: Rect,
    /// Drawn with Bézier curves: arcs, rings and other shapes no table uses.
    pub curved: bool,
}

#[derive(Debug, Default)]
pub(super) struct PageGeometry {
    pub width: f32,
    pub height: f32,
    pub hrules: Vec<Rule>,
    pub vrules: Vec<Rule>,
    pub fills: Vec<Fill>,
    pub images: Vec<Rect>,
}

type Matrix = [f32; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn apply(m: &Matrix, x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

#[derive(Clone, Copy)]
struct State {
    ctm: Matrix,
    /// Whether fills and strokes currently paint in (near) white, which on a
    /// white page draws nothing a reader sees.
    fill_white: bool,
    stroke_white: bool,
}

struct Walker<'d> {
    doc: &'d Document,
    geometry: PageGeometry,
    operations: usize,
    forms: HashSet<ObjectId>,
}

/// The shapes page `page_id` draws, rules merged.
pub(super) fn page_geometry(doc: &Document, page_id: ObjectId) -> PageGeometry {
    let (width, height) = page_size(doc, page_id);
    let mut walker = Walker {
        doc,
        geometry: PageGeometry { width, height, ..Default::default() },
        operations: 0,
        forms: HashSet::new(),
    };
    let resources = doc.get_page_resources(page_id).ok().and_then(|(inline, ids)| {
        inline.or_else(|| ids.first().and_then(|id| doc.get_dictionary(*id).ok()))
    });
    if let Ok(content) = doc.get_page_content(page_id)
        && let Ok(content) = Content::decode(&content)
    {
        let state = State { ctm: IDENTITY, fill_white: false, stroke_white: false };
        walker.run(&content, resources, state, 0);
    }
    let mut geometry = walker.geometry;
    geometry.hrules = merge_rules(std::mem::take(&mut geometry.hrules));
    geometry.vrules = merge_rules(std::mem::take(&mut geometry.vrules));
    geometry.fills = merge_tiles(std::mem::take(&mut geometry.fills));
    geometry
}

/// Join side-by-side fills of one height into one: a shaded table row is
/// often painted cell by cell.
fn merge_tiles(fills: Vec<Fill>) -> Vec<Fill> {
    fn gap(a: &Fill, b: &Fill) -> f32 {
        if a.rect.height() <= 30.0 && b.rect.height() <= 30.0 { 12.0 } else { 1.0 }
    }
    let mut out: Vec<Fill> = Vec::new();
    for fill in fills {
        let tile = out.iter_mut().rev().take(64).find(|f| {
            !f.curved
                && !fill.curved
                && (f.rect.y0 - fill.rect.y0).abs() <= 0.6
                && (f.rect.y1 - fill.rect.y1).abs() <= 0.6
                // Shaded rows leave a sliver of white between cells.
                && fill.rect.x0 <= f.rect.x1 + gap(f, &fill)
                && fill.rect.x1 >= f.rect.x0 - gap(f, &fill)
        });
        match tile {
            Some(f) => f.rect = f.rect.union(&fill.rect),
            None => out.push(fill),
        }
    }
    out
}

fn page_size(doc: &Document, page_id: ObjectId) -> (f32, f32) {
    let media = doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|page| page.get(b"MediaBox").ok())
        .and_then(|b| doc.dereference(b).ok())
        .and_then(|(_, b)| b.as_array().ok())
        .and_then(|b| {
            let n: Vec<f32> = b.iter().filter_map(|v| v.as_float().ok()).collect();
            (n.len() == 4).then(|| ((n[2] - n[0]).abs(), (n[3] - n[1]).abs()))
        });
    media.unwrap_or((612.0, 792.0))
}

fn number(object: &Object) -> Option<f32> {
    object.as_float().ok()
}

fn numbers(operands: &[Object]) -> Option<Vec<f32>> {
    operands.iter().map(number).collect()
}

/// Exactly `N` numeric operands.
fn fixed<const N: usize>(operands: &[Object]) -> Option<[f32; N]> {
    numbers(operands)?.try_into().ok()
}

/// Whether color operands paint white (or near it).
fn is_white(operator: &str, operands: &[Object]) -> bool {
    let Some(values) = numbers(operands) else { return false };
    match operator {
        "g" | "G" => values.first().is_some_and(|v| *v > 0.97),
        "rg" | "RG" => values.len() == 3 && values.iter().all(|v| *v > 0.97),
        "k" | "K" => values.len() == 4 && values.iter().all(|v| *v < 0.03),
        // `sc`/`scn` in an unknown space: only an all-ones tuple of one or
        // three components is read as white.
        _ => matches!(values.len(), 1 | 3) && values.iter().all(|v| *v > 0.97),
    }
}

#[derive(Default)]
struct Path {
    subpaths: Vec<Vec<(f32, f32)>>,
    curved: bool,
    /// Subpaths explicitly closed (`h`, `re`), whose stroke includes the
    /// closing edge.
    closed: Vec<bool>,
}

impl<'d> Walker<'d> {
    fn run(
        &mut self,
        content: &Content,
        resources: Option<&'d Dictionary>,
        start: State,
        depth: usize,
    ) {
        let mut state = start;
        let mut stack: Vec<State> = Vec::new();
        let mut path = Path::default();
        for op in &content.operations {
            self.operations += 1;
            if self.operations > MAX_OPERATIONS {
                return;
            }
            let operands = &op.operands;
            match op.operator.as_str() {
                "q" => {
                    if stack.len() < 256 {
                        stack.push(state);
                    }
                }
                "Q" => {
                    if let Some(saved) = stack.pop() {
                        state = saved;
                    }
                }
                "cm" => {
                    if let Some(m) = numbers(operands).filter(|m| m.len() == 6) {
                        state.ctm = multiply(&[m[0], m[1], m[2], m[3], m[4], m[5]], &state.ctm);
                    }
                }
                "g" | "rg" | "k" | "sc" | "scn" => {
                    state.fill_white = is_white(&op.operator, operands)
                }
                "G" | "RG" | "K" | "SC" | "SCN" => {
                    state.stroke_white = is_white(&op.operator, operands)
                }
                "m" => {
                    if let Some([x, y]) = fixed(operands) {
                        path.subpaths.push(vec![apply(&state.ctm, x, y)]);
                        path.closed.push(false);
                    }
                }
                "l" => {
                    if let Some([x, y]) = fixed(operands)
                        && let Some(sub) = path.subpaths.last_mut()
                    {
                        sub.push(apply(&state.ctm, x, y));
                    }
                }
                "c" | "v" | "y" => {
                    path.curved = true;
                    // The end point keeps the outline's extent roughly right.
                    if let Some(n) = numbers(operands)
                        && n.len() >= 4
                        && let Some(sub) = path.subpaths.last_mut()
                    {
                        let (x, y) = (n[n.len() - 2], n[n.len() - 1]);
                        for pair in n.chunks_exact(2) {
                            sub.push(apply(&state.ctm, pair[0], pair[1]));
                        }
                        sub.push(apply(&state.ctm, x, y));
                    }
                }
                "h" => {
                    if let Some(closed) = path.closed.last_mut() {
                        *closed = true;
                    }
                }
                "re" => {
                    if let Some([x, y, w, h]) = fixed(operands) {
                        let corners = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
                        path.subpaths
                            .push(corners.iter().map(|&(a, b)| apply(&state.ctm, a, b)).collect());
                        path.closed.push(true);
                    }
                }
                "S" | "s" => {
                    if op.operator == "s"
                        && let Some(closed) = path.closed.last_mut()
                    {
                        *closed = true;
                    }
                    if !state.stroke_white {
                        self.stroke(&path);
                    }
                    path = Path::default();
                }
                "f" | "F" | "f*" => {
                    if !state.fill_white {
                        self.fill(&path);
                    }
                    path = Path::default();
                }
                "B" | "B*" | "b" | "b*" => {
                    if !state.fill_white {
                        self.fill(&path);
                    }
                    if !state.stroke_white {
                        self.stroke(&path);
                    }
                    path = Path::default();
                }
                "n" => path = Path::default(),
                "Do" => {
                    let Some(name) = operands.first().and_then(|o| o.as_name().ok()) else {
                        continue;
                    };
                    self.draw_xobject(name, resources, state, depth);
                }
                _ => {}
            }
        }
    }

    fn draw_xobject(
        &mut self,
        name: &[u8],
        resources: Option<&'d Dictionary>,
        state: State,
        depth: usize,
    ) {
        let doc = self.doc;
        let Some(xobjects) = resources
            .and_then(|r| r.get(b"XObject").ok())
            .and_then(|x| doc.dereference(x).ok())
            .and_then(|(_, x)| x.as_dict().ok())
        else {
            return;
        };
        let Ok(reference) = xobjects.get(name) else { return };
        let Ok((id, object)) = doc.dereference(reference) else { return };
        let Ok(stream) = object.as_stream() else { return };
        match stream.dict.get(b"Subtype").and_then(Object::as_name) {
            Ok(b"Image") => {
                let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
                if let Some(rect) = bbox(corners.iter().map(|&(x, y)| apply(&state.ctm, x, y))) {
                    self.geometry.images.push(rect);
                }
            }
            Ok(b"Form") => {
                if depth >= MAX_FORM_DEPTH || id.is_some_and(|id| !self.forms.insert(id)) {
                    return;
                }
                let matrix = stream
                    .dict
                    .get(b"Matrix")
                    .ok()
                    .and_then(|m| m.as_array().ok())
                    .and_then(|m| numbers(m))
                    .filter(|m| m.len() == 6)
                    .map_or(IDENTITY, |m| [m[0], m[1], m[2], m[3], m[4], m[5]]);
                let inner = stream
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|r| doc.dereference(r).ok())
                    .and_then(|(_, r)| r.as_dict().ok())
                    .or(resources);
                let Ok(content) =
                    stream.decompressed_content().or_else(|_| Ok::<_, ()>(stream.content.clone()))
                else {
                    return;
                };
                if let Ok(content) = Content::decode(&content) {
                    let state = State { ctm: multiply(&matrix, &state.ctm), ..state };
                    self.run(&content, inner, state, depth + 1);
                }
                if let Some(id) = id {
                    self.forms.remove(&id);
                }
            }
            _ => {}
        }
    }

    fn stroke(&mut self, path: &Path) {
        for (points, closed) in path.subpaths.iter().zip(&path.closed) {
            if path.curved {
                // Stroked curves (ribbons, arcs, connectors) draw figures.
                if let Some(rect) = bbox(points.iter().copied())
                    && rect.width().max(rect.height()) >= 20.0
                {
                    self.geometry.fills.push(Fill { rect, curved: true });
                }
                continue;
            }
            let mut edges: Vec<((f32, f32), (f32, f32))> =
                points.windows(2).map(|w| (w[0], w[1])).collect();
            if *closed && points.len() > 2 {
                edges.push((points[points.len() - 1], points[0]));
            }
            for ((x0, y0), (x1, y1)) in edges {
                let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
                if dy <= 0.5 && dx >= 3.0 {
                    self.geometry.hrules.push(Rule {
                        at: (y0 + y1) / 2.0,
                        from: x0.min(x1),
                        to: x0.max(x1),
                    });
                } else if dx <= 0.5 && dy >= 3.0 {
                    self.geometry.vrules.push(Rule {
                        at: (x0 + x1) / 2.0,
                        from: y0.min(y1),
                        to: y0.max(y1),
                    });
                }
            }
        }
    }

    fn fill(&mut self, path: &Path) {
        let page_area = self.geometry.width * self.geometry.height;
        for points in &path.subpaths {
            let Some(rect) = bbox(points.iter().copied()) else { continue };
            let (w, h) = (rect.width(), rect.height());
            if !path.curved && h <= RULE_THICKNESS && w >= 3.0 {
                self.geometry.hrules.push(Rule {
                    at: (rect.y0 + rect.y1) / 2.0,
                    from: rect.x0,
                    to: rect.x1,
                });
            } else if !path.curved && w <= RULE_THICKNESS && h >= 3.0 {
                self.geometry.vrules.push(Rule {
                    at: (rect.x0 + rect.x1) / 2.0,
                    from: rect.y0,
                    to: rect.y1,
                });
            } else if w > 1.0 && h > 1.0 && w * h < page_area * 0.8 {
                // Page-sized fills are backgrounds, not shapes.
                self.geometry.fills.push(Fill { rect, curved: path.curved });
            }
        }
    }
}

fn bbox(points: impl Iterator<Item = (f32, f32)>) -> Option<Rect> {
    let mut rect: Option<Rect> = None;
    for (x, y) in points {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let point = Rect { x0: x, y0: y, x1: x, y1: y };
        rect = Some(rect.map_or(point, |r| r.union(&point)));
    }
    rect
}

/// Join collinear rules that touch or overlap: a rule drawn as a run of
/// segments (one per table cell, say) is one rule.
fn merge_rules(mut rules: Vec<Rule>) -> Vec<Rule> {
    rules.sort_by(|a, b| a.at.total_cmp(&b.at).then(a.from.total_cmp(&b.from)));
    let mut merged: Vec<Rule> = Vec::new();
    for rule in rules {
        match merged.last_mut() {
            Some(last) if (last.at - rule.at).abs() <= 1.0 && rule.from <= last.to + 1.5 => {
                last.to = last.to.max(rule.to);
            }
            _ => merged.push(rule),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collinear_segments_merge_into_one_rule() {
        let rules = merge_rules(vec![
            Rule { at: 10.0, from: 0.0, to: 50.0 },
            Rule { at: 10.4, from: 50.5, to: 90.0 },
            Rule { at: 30.0, from: 0.0, to: 90.0 },
        ]);
        assert_eq!(
            rules,
            vec![Rule { at: 10.0, from: 0.0, to: 90.0 }, Rule { at: 30.0, from: 0.0, to: 90.0 }]
        );
    }

    #[test]
    fn scaled_thin_rectangles_are_rules() {
        let mut doc = Document::with_version("1.4");
        let content = b"q 0.12 0 0 0.12 0 0 cm 473 6706 4135 5 re f 2449 5367 4 865 re f 100 100 800 400 re f Q".to_vec();
        let mut walker = Walker {
            doc: &doc,
            geometry: PageGeometry { width: 595.0, height: 842.0, ..Default::default() },
            operations: 0,
            forms: HashSet::new(),
        };
        let content = Content::decode(&content).unwrap();
        walker.run(
            &content,
            None,
            State { ctm: IDENTITY, fill_white: false, stroke_white: false },
            0,
        );
        let g = walker.geometry;
        assert_eq!(g.hrules.len(), 1);
        assert!((g.hrules[0].at - 804.9).abs() < 0.5);
        assert_eq!(g.vrules.len(), 1);
        assert!((g.vrules[0].at - 294.1).abs() < 0.5);
        assert_eq!(g.fills.len(), 1);
        doc.objects.clear();
    }
}
