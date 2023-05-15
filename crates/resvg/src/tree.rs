// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use usvg::{FuzzyEq, NodeExt};

use crate::clip::ClipPath;
use crate::image::Image;
use crate::mask::Mask;
use crate::paint_server::Paint;
use crate::path::{FillPath, StrokePath};

pub struct Group {
    pub opacity: f32,
    pub blend_mode: tiny_skia::BlendMode,
    pub clip_path: Option<ClipPath>,
    pub mask: Option<Mask>,

    pub filters: Vec<crate::filter::Filter>,
    pub filter_fill: Option<Paint>,
    pub filter_stroke: Option<Paint>,
    /// Group's layer bounding box in canvas coordinates.
    pub bbox: usvg::PathBbox,

    pub children: Vec<Node>,
}

pub enum Node {
    Group(Group), // TODO: box
    FillPath(FillPath),
    StrokePath(StrokePath),
    Image(Image),
}

/// A render tree.
///
/// - No hidden nodes.
/// - No text.
/// - Uses mostly tiny-skia types.
/// - No paint-order. Already resolved.
/// - PNG/JPEG/GIF bitmaps are already decoded and are stored as tiny_skia::Pixmap.
///   SVG images will be rendered each time.
/// - No `objectBoundingBox` units.
pub struct Tree {
    /// Image size.
    ///
    /// Size of an image that should be created to fit the SVG.
    ///
    /// `width` and `height` in SVG.
    pub size: usvg::Size,

    /// SVG viewbox.
    ///
    /// Specifies which part of the SVG image should be rendered.
    ///
    /// `viewBox` and `preserveAspectRatio` in SVG.
    pub view_box: usvg::ViewBox,

    /// Content area.
    ///
    /// A bounding box of all elements. Includes strokes and filter regions.
    ///
    /// Can be `None` when the tree has no children.
    pub content_area: Option<usvg::PathBbox>,

    pub(crate) children: Vec<Node>,
}

impl Tree {
    /// Creates a rendering tree from [`usvg::Tree`].
    ///
    /// Text nodes should be already converted into paths using
    /// [`usvg::TreeTextToPath::convert_text`].
    pub fn from_usvg(tree: &usvg::Tree) -> Self {
        if tree.has_text_nodes() {
            log::warn!("Text nodes should be already converted into paths.");
        }

        let (children, layer_bbox) =
            convert_node(tree.root.clone(), tiny_skia::Transform::default());

        Self {
            size: tree.size,
            view_box: tree.view_box,
            content_area: layer_bbox,
            children,
        }
    }

    /// Creates a rendering tree from [`usvg::Node`].
    ///
    /// Text nodes should be already converted into paths using
    /// [`usvg::TreeTextToPath::convert_text`].
    ///
    /// Returns `None` when `node` has a zero size.
    pub fn from_usvg_node(node: &usvg::Node) -> Option<Self> {
        let node_bbox = if let Some(bbox) = node.calculate_bbox().and_then(|r| r.to_rect()) {
            bbox
        } else {
            log::warn!("Node '{}' has zero size.", node.id());
            return None;
        };

        let view_box = usvg::ViewBox {
            rect: node_bbox,
            aspect: usvg::AspectRatio::default(),
        };

        let (children, layer_bbox) = convert_node(node.clone(), tiny_skia::Transform::default());

        Some(Self {
            size: node_bbox.size(),
            view_box: view_box,
            content_area: layer_bbox,
            children,
        })
    }
}

pub fn convert_node(
    node: usvg::Node,
    transform: tiny_skia::Transform,
) -> (Vec<Node>, Option<usvg::PathBbox>) {
    let mut children = Vec::new();
    let bboxes = convert_node_inner(node, transform, &mut children);
    (children, bboxes.map(|b| b.0))
}

fn convert_node_inner(
    node: usvg::Node,
    parent_transform: tiny_skia::Transform,
    children: &mut Vec<Node>,
) -> Option<(usvg::PathBbox, usvg::PathBbox)> {
    match &*node.borrow() {
        usvg::NodeKind::Group(ref ugroup) => {
            convert_group(node.clone(), ugroup, parent_transform, children)
        }
        usvg::NodeKind::Path(ref upath) => crate::path::convert(upath, parent_transform, children),
        usvg::NodeKind::Image(ref uimage) => {
            crate::image::convert(uimage, parent_transform, children)
        }
        usvg::NodeKind::Text(_) => None, // should be already converted into paths
    }
}

fn convert_group(
    node: usvg::Node,
    ugroup: &usvg::Group,
    parent_transform: tiny_skia::Transform,
    children: &mut Vec<Node>,
) -> Option<(usvg::PathBbox, usvg::PathBbox)> {
    let transform = parent_transform.pre_concat(ugroup.transform.to_native());

    if !ugroup.should_isolate() {
        return convert_children(node.clone(), transform, children);
    }

    let mut group_children = Vec::new();
    let (mut layer_bbox, object_bbox) = match convert_children(node, transform, &mut group_children)
    {
        Some(v) => v,
        None => return convert_empty_group(ugroup, transform, children),
    };

    let (filters, filter_bbox) = crate::filter::convert(
        &ugroup.filters,
        Some(layer_bbox),
        Some(object_bbox),
        transform,
    );

    // TODO: figure out a nicer solution
    // Ignore groups with filters but invalid filter bboxes.
    if !ugroup.filters.is_empty() && filter_bbox.is_none() {
        return None;
    }

    if let Some(filter_bbox) = filter_bbox {
        layer_bbox = filter_bbox;
    }

    let mut filter_fill = None;
    if let Some(ref paint) = ugroup.filter_fill {
        filter_fill =
            crate::paint_server::convert(&paint, usvg::Opacity::ONE, layer_bbox.to_skia_rect());
    }

    let mut filter_stroke = None;
    if let Some(ref paint) = ugroup.filter_stroke {
        filter_stroke =
            crate::paint_server::convert(&paint, usvg::Opacity::ONE, layer_bbox.to_skia_rect());
    }

    let group = Group {
        opacity: ugroup.opacity.get() as f32,
        blend_mode: convert_blend_mode(ugroup.blend_mode),
        clip_path: crate::clip::convert(ugroup.clip_path.clone(), object_bbox, transform),
        mask: crate::mask::convert(ugroup.mask.clone(), object_bbox, transform),
        filters,
        filter_fill,
        filter_stroke,
        bbox: layer_bbox,
        children: group_children,
    };

    children.push(Node::Group(group));
    Some((layer_bbox, object_bbox))
}

fn convert_empty_group(
    ugroup: &usvg::Group,
    transform: tiny_skia::Transform,
    children: &mut Vec<Node>,
) -> Option<(usvg::PathBbox, usvg::PathBbox)> {
    if ugroup.filters.is_empty() {
        return None;
    }

    let (filters, layer_bbox) = crate::filter::convert(&ugroup.filters, None, None, transform);
    let layer_bbox = layer_bbox?;

    let mut filter_fill = None;
    if let Some(ref paint) = ugroup.filter_fill {
        filter_fill =
            crate::paint_server::convert(&paint, usvg::Opacity::ONE, layer_bbox.to_skia_rect());
    }

    let mut filter_stroke = None;
    if let Some(ref paint) = ugroup.filter_stroke {
        filter_stroke =
            crate::paint_server::convert(&paint, usvg::Opacity::ONE, layer_bbox.to_skia_rect());
    }

    let group = Group {
        opacity: ugroup.opacity.get() as f32,
        blend_mode: convert_blend_mode(ugroup.blend_mode),
        clip_path: None,
        mask: None,
        filters,
        filter_fill,
        filter_stroke,
        bbox: layer_bbox,
        children: Vec::new(),
    };

    // TODO: find a better solution
    let object_bbox = usvg::PathBbox::new(0.0, 0.0, 1.0, 1.0).unwrap();

    children.push(Node::Group(group));
    Some((layer_bbox, object_bbox))
}

fn convert_children(
    parent: usvg::Node,
    parent_transform: tiny_skia::Transform,
    children: &mut Vec<Node>,
) -> Option<(usvg::PathBbox, usvg::PathBbox)> {
    let mut layer_bbox = usvg::PathBbox::new_bbox();
    let mut object_bbox = usvg::PathBbox::new_bbox();

    for node in parent.children() {
        if let Some((node_layer_bbox, node_object_bbox)) =
            convert_node_inner(node, parent_transform, children)
        {
            object_bbox = object_bbox.expand(node_object_bbox);
            layer_bbox = layer_bbox.expand(node_layer_bbox);
        }
    }

    if layer_bbox.fuzzy_ne(&usvg::PathBbox::new_bbox())
        && object_bbox.fuzzy_ne(&usvg::PathBbox::new_bbox())
    {
        Some((layer_bbox, object_bbox))
    } else {
        None
    }
}

pub fn convert_blend_mode(mode: usvg::BlendMode) -> tiny_skia::BlendMode {
    match mode {
        usvg::BlendMode::Normal => tiny_skia::BlendMode::SourceOver,
        usvg::BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
        usvg::BlendMode::Screen => tiny_skia::BlendMode::Screen,
        usvg::BlendMode::Overlay => tiny_skia::BlendMode::Overlay,
        usvg::BlendMode::Darken => tiny_skia::BlendMode::Darken,
        usvg::BlendMode::Lighten => tiny_skia::BlendMode::Lighten,
        usvg::BlendMode::ColorDodge => tiny_skia::BlendMode::ColorDodge,
        usvg::BlendMode::ColorBurn => tiny_skia::BlendMode::ColorBurn,
        usvg::BlendMode::HardLight => tiny_skia::BlendMode::HardLight,
        usvg::BlendMode::SoftLight => tiny_skia::BlendMode::SoftLight,
        usvg::BlendMode::Difference => tiny_skia::BlendMode::Difference,
        usvg::BlendMode::Exclusion => tiny_skia::BlendMode::Exclusion,
        usvg::BlendMode::Hue => tiny_skia::BlendMode::Hue,
        usvg::BlendMode::Saturation => tiny_skia::BlendMode::Saturation,
        usvg::BlendMode::Color => tiny_skia::BlendMode::Color,
        usvg::BlendMode::Luminosity => tiny_skia::BlendMode::Luminosity,
    }
}

pub trait OptionLog {
    fn log_none<F: FnOnce()>(self, f: F) -> Self;
}

impl<T> OptionLog for Option<T> {
    #[inline]
    fn log_none<F: FnOnce()>(self, f: F) -> Self {
        self.or_else(|| {
            f();
            None
        })
    }
}

pub trait ConvTransform {
    fn to_native(&self) -> tiny_skia::Transform;
    fn from_native(_: tiny_skia::Transform) -> Self;
}

impl ConvTransform for usvg::Transform {
    fn to_native(&self) -> tiny_skia::Transform {
        tiny_skia::Transform::from_row(
            self.a as f32,
            self.b as f32,
            self.c as f32,
            self.d as f32,
            self.e as f32,
            self.f as f32,
        )
    }

    fn from_native(ts: tiny_skia::Transform) -> Self {
        Self::new(
            ts.sx as f64,
            ts.ky as f64,
            ts.kx as f64,
            ts.sy as f64,
            ts.tx as f64,
            ts.ty as f64,
        )
    }
}

pub trait TinySkiaRectExt {
    fn to_path_bbox(&self) -> Option<usvg::PathBbox>;
}

impl TinySkiaRectExt for tiny_skia::Rect {
    fn to_path_bbox(&self) -> Option<usvg::PathBbox> {
        usvg::PathBbox::new(
            self.x() as f64,
            self.y() as f64,
            self.width() as f64,
            self.height() as f64,
        )
    }
}

pub trait UsvgRectExt {
    fn to_skia_rect(&self) -> Option<tiny_skia::Rect>;
}

impl UsvgRectExt for usvg::Rect {
    fn to_skia_rect(&self) -> Option<tiny_skia::Rect> {
        tiny_skia::Rect::from_xywh(
            self.x() as f32,
            self.y() as f32,
            self.width() as f32,
            self.height() as f32,
        )
    }
}

pub trait UsvgPathBboxExt {
    fn to_skia_rect(&self) -> tiny_skia::Rect;
}

impl UsvgPathBboxExt for usvg::PathBbox {
    fn to_skia_rect(&self) -> tiny_skia::Rect {
        tiny_skia::Rect::from_xywh(
            self.x() as f32,
            self.y() as f32,
            self.width() as f32,
            self.height() as f32,
        )
        .unwrap()
    }
}

pub trait TinySkiaTransformExt {
    fn from_bbox(bbox: usvg::Rect) -> tiny_skia::Transform;
}

impl TinySkiaTransformExt for tiny_skia::Transform {
    fn from_bbox(bbox: usvg::Rect) -> tiny_skia::Transform {
        tiny_skia::Transform::from_row(
            bbox.width() as f32,
            0.0,
            0.0,
            bbox.height() as f32,
            bbox.x() as f32,
            bbox.y() as f32,
        )
    }
}