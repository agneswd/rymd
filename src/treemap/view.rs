//! Treemap rendering.
//!
//! [`TreemapElement`] is a custom-painted control: rectangles are drawn as
//! quads at exact pixel bounds and labels are shaped once per layout and
//! painted directly. Nothing runs taffy layout per tile, so a
//! thousand-tile treemap costs about the same as ten.
//!
//! Highlights stroke the inside of each tile's own bounds, which means
//! edge and corner tiles can never have their ring clipped by the
//! container mask. Model data (names, sizes) is read only when the layout
//! cache rebuilds; hover tooltips resolve their path lazily at display
//! time.

use std::sync::Arc;

use gpui::{
    App, Bounds, Corners, Edges, Element, ElementId, GlobalElementId, InspectorElementId,
    InteractiveElement as _, IntoElement, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    SharedString, Size, Styled, Window, point, px,
};
use parking_lot::RwLock;

use gpui_component::ActiveTheme as _;

use crate::model::{HARDLINK_SHARED, MOUNT_BOUNDARY, NodeId, ScanModel};
use crate::scan::options::SizeMetric;
use crate::ui::shell::AppShell;
use crate::util::format_size::{format_count, format_percent, format_size};

use super::layout::{FRect, TreemapItem, TreemapRect, squarify};

/// Rectangles under this many pixels on a side are not drawn.
const MIN_VISIBLE: f32 = 3.0;
/// Padding between the container edge and the outermost rectangles.
const EDGE_PAD: f32 = 4.0;
/// Hard cap on rendered rectangles so huge folders stay cheap to draw.
const MAX_RECTS: usize = 1200;

pub struct TreemapElement {
    /// Styling container (border, rounding, background, hit area).
    base: gpui::Stateful<gpui::Div>,
    items: Vec<TreemapItem>,
    model: Arc<RwLock<ScanModel>>,
    dir_total: u64,
    metric: SizeMetric,
    selected: Option<u32>,
    /// Node id currently under the mouse, tracked by the shell.
    hovered: Option<u32>,
    shell: gpui::WeakEntity<AppShell>,
    /// Bumped by the shell whenever inputs behind the weights change
    /// (navigation, filter, metric, deletions). Part of the cache key so
    /// look-alike directories can never serve stale rectangles.
    version: u64,
}

impl TreemapElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        items: Vec<TreemapItem>,
        model: Arc<RwLock<ScanModel>>,
        dir_total: u64,
        metric: SizeMetric,
        selected: Option<u32>,
        hovered: Option<u32>,
        shell: gpui::WeakEntity<AppShell>,
        version: u64,
    ) -> Self {
        Self {
            base: gpui::div().id("treemap").size_full().relative(),
            items,
            model,
            dir_total,
            metric,
            selected,
            hovered,
            shell,
            version,
        }
    }
}

impl IntoElement for TreemapElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl ParentElement for TreemapElement {
    fn extend(&mut self, _: impl IntoIterator<Item = gpui::AnyElement>) {}
}

impl Styled for TreemapElement {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.base.style()
    }
}

/// Inputs that force a relayout when they change.
#[derive(Clone, Copy, PartialEq)]
pub struct LayoutKey {
    width: f32,
    height: f32,
    version: u64,
}

/// Per-rectangle display data, cached alongside the geometry so ordinary
/// frames never touch the model.
#[derive(Clone)]
pub struct RectVisual {
    name: SharedString,
    sublabel: Option<SharedString>,
    level: usize,
    selected: bool,
}

#[derive(Default, Clone)]
pub struct TreemapState {
    key: Option<LayoutKey>,
    rects: Vec<TreemapRect>,
    visuals: Vec<RectVisual>,
    /// Shaped labels keyed by rect index; cleared on relayout.
    labels: std::collections::HashMap<usize, gpui::ShapedLine>,
}

pub struct PreparedChildren {}

impl Element for TreemapElement {
    type RequestLayoutState = PreparedChildren;
    type PrepaintState = Option<TreemapState>;

    fn id(&self) -> Option<ElementId> {
        Some("treemap".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let layout_id = self.base.interactivity().request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |style, window, cx| window.request_layout(style, None, cx),
        );
        (layout_id, PreparedChildren {})
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let Some(global_id) = global_id else {
            self.base.interactivity().prepaint(
                global_id,
                inspector_id,
                bounds,
                bounds.size,
                window,
                cx,
                |_style, _origin, _hitbox, _window, _cx| {},
            );
            return None;
        };
        let width = f32::from(bounds.size.width.max(px(0.0)));
        let height = f32::from(bounds.size.height.max(px(0.0)));

        // Build or load the cached geometry + visuals. The only model read
        // in steady-state frames happens inside this closure when the key
        // actually changed.
        let state: TreemapState =
            window.with_element_state(global_id, |stored: Option<TreemapState>, _| {
                let mut state: TreemapState = stored.unwrap_or_default();
                let key = LayoutKey {
                    width,
                    height,
                    version: self.version,
                };
                if state.key != Some(key) {
                    state.rects = squarify(
                        &self.items,
                        FRect::new(
                            EDGE_PAD,
                            EDGE_PAD,
                            (width - 2.0 * EDGE_PAD).max(0.0),
                            (height - 2.0 * EDGE_PAD).max(0.0),
                        ),
                    );
                    state.rects.truncate(MAX_RECTS);
                    let metric = self.metric;
                    let dir_total = self.dir_total;
                    let selected = self.selected;
                    let model = self.model.clone();
                    state.visuals = state
                        .rects
                        .iter()
                        .map(|r| {
                            let node_id = NodeId(r.node_id);
                            let (name, size, items) = {
                                let m = model.read();
                                let n = m.node(node_id);
                                (
                                    n.name.to_string_lossy().into_owned(),
                                    metric.pick(n.agg_logical, n.agg_allocated),
                                    n.file_count + n.dir_count,
                                )
                            };
                            let ratio = if dir_total > 0 {
                                (size as f32 / dir_total as f32).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            let level = if ratio >= 0.5 {
                                4
                            } else if ratio >= 0.25 {
                                3
                            } else if ratio >= 0.1 {
                                2
                            } else if ratio >= 0.03 {
                                1
                            } else {
                                0
                            };
                            let sublabel = if r.h >= 52.0 && r.w >= 90.0 && size > 0 {
                                let mut s = format_size(size);
                                if items > 0 {
                                    s.push_str(" · ");
                                    s.push_str(&format_count(items));
                                }
                                Some(s.into())
                            } else {
                                None
                            };
                            RectVisual {
                                name: name.into(),
                                sublabel,
                                level,
                                selected: selected == Some(r.node_id),
                            }
                        })
                        .collect();
                    state.labels.clear();
                    state.key = Some(key);
                }
                let handed_out = state.clone();
                (handed_out, state)
            });

        self.base.interactivity().prepaint(
            Some(global_id),
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_style, _origin, _hitbox, _window, _cx| {},
        );

        Some(state)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(mut state) = prepaint.take() else {
            self.base.interactivity().paint(
                global_id,
                inspector_id,
                bounds,
                None,
                window,
                cx,
                |_, _, _| {},
            );
            return;
        };

        // Hover tracking: the shell owns the hovered node so the next
        // frame repaints with the new highlight and follows the cursor.
        let rects_hover = state.rects.clone();
        let current_hover = self.hovered;
        let shell_for_hover = self.shell.clone();
        window.on_mouse_event::<MouseMoveEvent>(move |event, phase, _window, cx| {
            if phase != gpui::DispatchPhase::Capture {
                return;
            }
            let hit_node = if bounds.contains(&event.position) {
                hit_test(&rects_hover, event.position - bounds.origin)
                    .map(|ix| rects_hover[ix].node_id)
            } else {
                None
            };
            if hit_node != current_hover {
                let _ = shell_for_hover.update(cx, |shell, cx| {
                    shell.treemap_hovered = hit_node;
                    cx.notify();
                });
            } else if hit_node.is_some() {
                let _ = shell_for_hover.update(cx, |_shell, cx| {
                    cx.notify();
                });
            }
        });

        // Click / double-click dispatch.
        let rects_click = state.rects.clone();
        let shell = self.shell.clone();
        window.on_mouse_event::<MouseUpEvent>(move |event, phase, _window, cx| {
            if phase != gpui::DispatchPhase::Capture || !bounds.contains(&event.position) {
                return;
            }
            let Some(hit) = hit_test(&rects_click, event.position - bounds.origin) else {
                return;
            };
            let node_id = NodeId(rects_click[hit].node_id);
            let double = event.click_count >= 2;
            let _ = shell.update(cx, |shell, cx| {
                if double {
                    shell.activate_node(node_id, cx);
                } else {
                    shell.select_node(node_id, cx);
                }
            });
        });

        // Paint inside the container's content mask.
        let theme = cx.theme();
        let primary = theme.primary;
        let foreground = theme.foreground;
        let muted = theme.muted_foreground;
        let background = theme.background;
        let popover = theme.popover;
        let border_col = theme.border;
        let font = gpui::Font {
            family: theme.font_family.clone(),
            features: Default::default(),
            fallbacks: None,
            weight: gpui::FontWeight::NORMAL,
            style: gpui::FontStyle::Normal,
        };

        self.base.interactivity().paint(
            global_id,
            inspector_id,
            bounds,
            None,
            window,
            cx,
            |_style, window, cx| {
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    let opacity = [0.16f32, 0.22, 0.29, 0.36, 0.44];
                    for (ix, r) in state.rects.iter().enumerate() {
                        if r.w < MIN_VISIBLE || r.h < MIN_VISIBLE {
                            continue;
                        }
                        let visual = &state.visuals[ix];
                        let tile_bounds = Bounds {
                            origin: bounds.origin + point(px(r.x), px(r.y)),
                            size: Size::new(px((r.w - 1.0).max(1.0)), px((r.h - 1.0).max(1.0))),
                        };
                        let fill_alpha = if visual.selected {
                            0.55
                        } else {
                            opacity[visual.level]
                        };
                        window.paint_quad(gpui::quad(
                            tile_bounds,
                            Corners::all(px(2.0)),
                            primary.opacity(fill_alpha),
                            Edges::default(),
                            background,
                            gpui::BorderStyle::Solid,
                        ));
                        // Inset ring: separator by default, accent when hot.
                        // Drawn strictly inside the tile bounds, so tiles
                        // touching any container edge keep their highlight.
                        let ring = if visual.selected || self.hovered == Some(r.node_id) {
                            primary
                        } else {
                            background
                        };
                        window.paint_quad(gpui::quad(
                            tile_bounds,
                            Corners::all(px(2.0)),
                            gpui::transparent_black(),
                            Edges::all(px(1.0)),
                            ring,
                            gpui::BorderStyle::Solid,
                        ));
                    }

                    // Labels where they fit.
                    for (ix, r) in state.rects.iter().enumerate() {
                        if r.w < MIN_VISIBLE || r.h < MIN_VISIBLE {
                            continue;
                        }
                        let show_full = r.h >= 38.0 && r.w >= 90.0;
                        let show_name_only = !show_full && r.w >= 56.0 && r.h >= 18.0;
                        if !show_full && !show_name_only {
                            continue;
                        }
                        let visual = &state.visuals[ix];
                        let tile_bounds = Bounds {
                            origin: bounds.origin + point(px(r.x), px(r.y)),
                            size: Size::new(px((r.w - 1.0).max(1.0)), px((r.h - 1.0).max(1.0))),
                        };
                        let tile_origin = bounds.origin + point(px(r.x), px(r.y));
                        window.with_content_mask(
                            Some(gpui::ContentMask {
                                bounds: tile_bounds,
                            }),
                            |window| {
                                if show_full {
                                    let line = state.labels.entry(ix).or_insert_with(|| {
                                        let run = gpui::TextRun {
                                            len: visual.name.len(),
                                            font: font.clone(),
                                            color: foreground,
                                            background_color: None,
                                            underline: None,
                                            strikethrough: None,
                                        };
                                        window.text_system().shape_line(
                                            visual.name.clone(),
                                            px(12.0),
                                            &[run],
                                            None,
                                        )
                                    });
                                    let origin = tile_origin + point(px(4.0), px(4.0));
                                    let _ = line.paint(
                                        origin,
                                        px(14.5),
                                        gpui::TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    );
                                    if let Some(sub) = &visual.sublabel {
                                        let run = gpui::TextRun {
                                            len: sub.len(),
                                            font: font.clone(),
                                            color: muted,
                                            background_color: None,
                                            underline: None,
                                            strikethrough: None,
                                        };
                                        let shaped = window.text_system().shape_line(
                                            sub.clone(),
                                            px(10.5),
                                            &[run],
                                            None,
                                        );
                                        let _ = shaped.paint(
                                            tile_origin + point(px(4.0), px(19.0)),
                                            px(13.0),
                                            gpui::TextAlign::Left,
                                            None,
                                            window,
                                            cx,
                                        );
                                    }
                                } else {
                                    let run = gpui::TextRun {
                                        len: visual.name.len(),
                                        font: font.clone(),
                                        color: foreground,
                                        background_color: None,
                                        underline: None,
                                        strikethrough: None,
                                    };
                                    let shaped = window.text_system().shape_line(
                                        visual.name.clone(),
                                        px(10.5),
                                        &[run],
                                        None,
                                    );
                                    let _ = shaped.paint(
                                        tile_origin + point(px(3.0), px(3.0)),
                                        px(12.0),
                                        gpui::TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    );
                                }
                            },
                        );
                    }

                    // Tooltip while hovering, positioned near the mouse cursor.
                    if let Some(hover_node) = self.hovered
                        && let Some(r) = state.rects.iter().find(|r| r.node_id == hover_node)
                    {
                        let node_id = NodeId(r.node_id);
                        let (name, size, items, is_dir, hardlink, mount, path) = {
                            let m = self.model.read();
                            let n = m.node(node_id);
                            (
                                n.name.to_string_lossy().into_owned(),
                                self.metric.pick(n.agg_logical, n.agg_allocated),
                                n.file_count + n.dir_count,
                                n.is_dir(),
                                n.flags & HARDLINK_SHARED != 0,
                                n.flags & MOUNT_BOUNDARY != 0,
                                m.path_of(node_id).to_string_lossy().into_owned(),
                            )
                        };
                        let mut lines: Vec<(SharedString, gpui::Hsla)> =
                            vec![(name.into(), foreground)];
                        lines.push((format_size(size).into(), muted));
                        if is_dir && self.dir_total > 0 {
                            lines.push((
                                format!(
                                    "{}, {} items",
                                    format_percent(size, self.dir_total),
                                    format_count(items)
                                )
                                .into(),
                                muted,
                            ));
                        }
                        if hardlink {
                            lines.push(("Hard link: storage counted elsewhere".into(), muted));
                        }
                        if mount {
                            lines.push(("Mount point: not scanned".into(), muted));
                        }
                        lines.push((path.into(), muted));
                        let mouse_pos = window.mouse_position();
                        let style = TooltipStyle {
                            bg: popover,
                            border: border_col,
                            font: &font,
                        };
                        paint_tooltip(window, cx, bounds, mouse_pos, style, &lines);
                    }
                });
            },
        );

        // Persist any label/hover mutations made above.
        if let Some(global_id) = global_id {
            window.with_element_state(global_id, |_: Option<TreemapState>, _| ((), state));
        }
    }
}

struct TooltipStyle<'a> {
    bg: gpui::Hsla,
    border: gpui::Hsla,
    font: &'a gpui::Font,
}

fn hit_test(rects: &[TreemapRect], pos: gpui::Point<Pixels>) -> Option<usize> {
    let x = f32::from(pos.x);
    let y = f32::from(pos.y);
    rects
        .iter()
        .position(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
}

fn paint_tooltip(
    window: &mut Window,
    cx: &mut App,
    container: Bounds<Pixels>,
    mouse_pos: gpui::Point<Pixels>,
    style: TooltipStyle,
    lines: &[(SharedString, gpui::Hsla)],
) {
    let pad = px(8.0);
    let line_h = px(16.0);
    let shaped_lines: Vec<_> = lines
        .iter()
        .map(|(text, color)| {
            let run = gpui::TextRun {
                len: text.len(),
                font: style.font.clone(),
                color: *color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(text.clone(), px(12.0), &[run], None)
        })
        .collect();

    let max_text_w = shaped_lines
        .iter()
        .map(|l| l.width)
        .fold(px(0.0), |a, b| a.max(b));
    let content_w = max_text_w + pad * 2.0;
    let max_w = px(420.0).min((container.size.width - px(16.0)).max(px(100.0)));
    let width = content_w.min(max_w).max(px(120.0));
    let height = line_h * lines.len() as f32 + pad * 2.0;

    let offset_x = px(12.0);
    let offset_y = px(16.0);
    let mut origin_x = mouse_pos.x + offset_x;
    let mut origin_y = mouse_pos.y + offset_y;

    let max_x = container.origin.x + container.size.width - width - px(6.0);
    if origin_x > max_x {
        let left_x = mouse_pos.x - width - px(12.0);
        if left_x >= container.origin.x + px(6.0) {
            origin_x = left_x;
        } else {
            origin_x = max_x;
        }
    }

    let max_y = container.origin.y + container.size.height - height - px(6.0);
    if origin_y > max_y {
        let top_y = mouse_pos.y - height - px(12.0);
        if top_y >= container.origin.y + px(6.0) {
            origin_y = top_y;
        } else {
            origin_y = max_y;
        }
    }

    origin_x = origin_x.clamp(
        container.origin.x + px(6.0),
        (container.origin.x + container.size.width - width - px(6.0))
            .max(container.origin.x + px(6.0)),
    );
    origin_y = origin_y.clamp(
        container.origin.y + px(6.0),
        (container.origin.y + container.size.height - height - px(6.0))
            .max(container.origin.y + px(6.0)),
    );

    let card = Bounds {
        origin: point(origin_x, origin_y),
        size: Size::new(width, height),
    };
    window.paint_quad(gpui::quad(
        card,
        Corners::all(px(4.0)),
        style.bg,
        Edges::all(px(1.0)),
        style.border,
        gpui::BorderStyle::Solid,
    ));
    let mut y = origin_y + pad;
    for shaped in shaped_lines {
        let _ = shaped.paint(
            point(origin_x + pad, y),
            line_h,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        y += line_h;
    }
}
