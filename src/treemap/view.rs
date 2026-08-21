//! Treemap rendering.
//!
//! [`TreemapElement`] is the one custom visual control in the app. It
//! measures its container during prepaint (the same approach gpui-component's
//! virtual list uses), runs the pure squarify layout from `layout.rs`, and
//! then lays out and paints one interactive `div` per rectangle.

use std::rc::Rc;

use gpui::{
    div, px, AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, ParentElement, Pixels, Point, Size, Stateful, point,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window,
};
use gpui_component::{tooltip::Tooltip, ActiveTheme as _};
use parking_lot::RwLock;

use crate::model::{NodeId, ScanModel, HARDLINK_SHARED, MOUNT_BOUNDARY};
use crate::scan::options::SizeMetric;
use crate::ui::shell::AppShell;
use crate::util::format_size::{format_count, format_percent, format_size};

use super::layout::{squarify, FRect, TreemapItem, TreemapRect};

/// Rectangles under this many pixels on a side are not drawn.
const MIN_VISIBLE: f32 = 3.0;
/// Padding between the container edge and the outermost rectangles, so
/// selection outlines and labels never get clipped.
const EDGE_PAD: f32 = 3.0;
/// Hard cap on rendered rectangles so huge folders stay cheap to draw.
const MAX_RECTS: usize = 2000;

pub struct TreemapElement {
    base: gpui::Stateful<gpui::Div>,
    items: Vec<TreemapItem>,
    model: Rc<RwLock<ScanModel>>,
    dir_total: u64,
    metric: SizeMetric,
    selected: Option<u32>,
    shell: WeakEntity<AppShell>,
}

impl TreemapElement {
    pub fn new(
        items: Vec<TreemapItem>,
        model: Rc<RwLock<ScanModel>>,
        dir_total: u64,
        metric: SizeMetric,
        selected: Option<u32>,
        shell: WeakEntity<AppShell>,
    ) -> Self {
        Self {
            base: div().id("treemap").size_full().relative(),
            items,
            model,
            dir_total,
            metric,
            selected,
            shell,
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
    fn extend(&mut self, _: impl IntoIterator<Item = AnyElement>) {}
}

impl Styled for TreemapElement {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.base.style()
    }
}

/// Inputs that force a relayout when they change.
#[derive(Clone, Copy, PartialEq)]
struct LayoutKey {
    width: f32,
    height: f32,
    weights_sum: u64,
    first_weight_bits: u64,
}

#[derive(Default)]
struct TreemapState {
    key: Option<LayoutKey>,
    rects: Vec<TreemapRect>,
}

pub struct PreparedChildren {
    pub children: Vec<(TreemapRect, AnyElement)>,
}

impl Element for TreemapElement {
    type RequestLayoutState = PreparedChildren;
    type PrepaintState = ();

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
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.base.interactivity().request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |style, window, cx| window.request_layout(style, None, cx),
        );
        (
            layout_id,
            PreparedChildren {
                children: Vec::new(),
            },
        )
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let width = f32::from(bounds.size.width.max(px(0.0)));
        let height = f32::from(bounds.size.height.max(px(0.0)));

        // Element state persists across frames; relayout only when the
        // inputs actually change.
        let rects =
            window.with_element_state(global_id.unwrap(), |state: Option<TreemapState>, _| {
                let mut state = state.unwrap_or_default();
                let weights_sum: u64 = self.items.iter().map(|i| i.weight as u64).sum();
                let key = LayoutKey {
                    width,
                    height,
                    weights_sum,
                    first_weight_bits: self.items.first().map(|i| i.weight.to_bits()).unwrap_or(0),
                };
                let unchanged = state.key == Some(key);
                if !unchanged {
                    state.rects = squarify(&self.items, FRect::new(0., 0., width, height));
                    state.key = Some(key);
                }

                let drawable = state
                    .rects
                    .iter()
                    .filter(|r| r.w >= MIN_VISIBLE && r.h >= MIN_VISIBLE)
                    .take(MAX_RECTS)
                    .copied()
                    .collect::<Vec<_>>();
                (drawable, state)
            });

        let mut prepared: Vec<(TreemapRect, AnyElement)> = rects
            .into_iter()
            .map(|r| {
                let el = self.render_rect(r, cx);
                (r, el)
            })
            .collect();

        self.base.interactivity().prepaint(
            global_id,
            inspector_id,
            bounds,
            bounds.size,
            window,
            cx,
            |_style, _origin, _hitbox, window, cx| {
                let mask = gpui::ContentMask { bounds };
                window.with_content_mask(Some(mask), |window| {
                    for (r, el) in prepared.drain(..) {
                        let mut el = el;
                        el.layout_as_root(
                            Size::new(
                                gpui::AvailableSpace::Definite(px(r.w)),
                                gpui::AvailableSpace::Definite(px(r.h)),
                            ),
                            window,
                            cx,
                        );
                        // prepaint_at takes window coordinates: offset the
                        // layout-relative rect by the container origin.
                        el.prepaint_at(bounds.origin + point(px(r.x), px(r.y)), window, cx);
                        request_layout.children.push((r, el));
                    }
                });
            },
        );
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.base.interactivity().paint(
            global_id,
            inspector_id,
            bounds,
            None,
            window,
            cx,
            |_, window, cx| {
                for (_, item) in &mut request_layout.children {
                    item.paint(window, cx);
                }
            },
        );
    }
}

impl TreemapElement {
    fn render_rect(&self, r: TreemapRect, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let node_id = NodeId(r.node_id);
        let info = {
            let m = self.model.read();
            let n = m.node(node_id);
            (
                n.name.to_string_lossy().into_owned(),
                self.metric.pick(n.agg_logical, n.agg_allocated),
                n.file_count + n.dir_count,
                m.path_of(node_id).to_string_lossy().into_owned(),
                n.is_dir(),
                n.flags & HARDLINK_SHARED != 0,
                n.flags & MOUNT_BOUNDARY != 0,
            )
        };
        let (name, size, items, path, is_dir, hardlink, mount) = info;

        // One accent color, five depth levels by share of the directory.
        let ratio = if self.dir_total > 0 {
            (size as f32 / self.dir_total as f32).clamp(0.0, 1.0)
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
        let opacity = [0.16f32, 0.22, 0.29, 0.36, 0.44][level];
        let selected = self.selected == Some(r.node_id);

        let fill = if selected {
            theme.primary.opacity(0.55)
        } else {
            theme.primary.opacity(opacity)
        };

        let show_full_label = r.h >= 38.0 && (r.w >= 90.0 || (is_dir && r.w >= 70.0));
        let show_name_only = !show_full_label && r.w >= 56.0 && r.h >= 18.0;

        let mut rect_div: Stateful<gpui::Div> = div()
            .id(("treemap-node", r.node_id as u64))
            .absolute()
            .left(px(r.x))
            .top(px(r.y))
            .w(px((r.w - 1.0).max(1.0)))
            .h(px((r.h - 1.0).max(1.0)))
            .bg(fill)
            .border_1()
            .border_color(theme.background)
            .rounded(px(2.0))
            .cursor_pointer()
            .hover(|s| s.border_color(theme.primary));

        if show_full_label {
            let mut label_col = div().flex().flex_col().p_1().overflow_hidden().child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .truncate()
                    .child(name.clone()),
            );
            if r.h >= 52.0 {
                label_col = label_col.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{}{}",
                            format_size(size),
                            if is_dir && items > 0 {
                                format!(" · {}", format_count(items))
                            } else {
                                String::new()
                            }
                        )),
                );
            }
            rect_div = rect_div.child(label_col);
        } else if show_name_only {
            rect_div = rect_div.child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(name.clone()),
            );
        }

        let shell = self.shell.clone();
        rect_div = rect_div.on_click(move |event, _, cx| {
            let double = matches!(
                event,
                gpui::ClickEvent::Mouse(m) if m.up.click_count >= 2
            );
            let _ = shell.update(cx, |shell, cx| {
                if double {
                    shell.activate_node(node_id, cx);
                } else {
                    shell.select_node(node_id, cx);
                }
            });
        });

        let dir_total = self.dir_total;
        let t_name = name.clone();
        let t_path = path.clone();
        rect_div = rect_div.tooltip(move |window, cx| {
            let n2 = t_name.clone();
            let p2 = t_path.clone();
            Tooltip::element(move |_, cx| {
                tooltip_body(
                    cx, &n2, size, items, is_dir, hardlink, mount, dir_total, &p2,
                )
            })
            .build(window, cx)
        });

        rect_div.into_any_element()
    }
}

fn tooltip_body(
    cx: &gpui::App,
    name: &str,
    size: u64,
    items: u64,
    is_dir: bool,
    hardlink: bool,
    mount: bool,
    dir_total: u64,
    path: &str,
) -> gpui::AnyElement {
    use gpui_component::{h_flex, v_flex};
    let theme = cx.theme();

    let mut col = v_flex()
        .gap_0p5()
        .child(
            h_flex().child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name.to_string()),
            ),
        )
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(format_size(size)),
        );

    if is_dir {
        col = col.child(div().text_color(theme.muted_foreground).child(format!(
            "{}, {} items",
            format_percent(size, dir_total),
            format_count(items)
        )));
    } else if dir_total > 0 {
        col = col.child(
            div()
                .text_color(theme.muted_foreground)
                .child(format_percent(size, dir_total)),
        );
    }
    if hardlink {
        col = col.child(
            div()
                .text_color(theme.warning)
                .child("Hard link: storage counted at another path"),
        );
    }
    if mount {
        col = col.child(
            div()
                .text_color(theme.info)
                .child("Mount point: contents not scanned"),
        );
    }
    col.child(
        div()
            .text_color(theme.muted_foreground)
            .text_size(px(11.0))
            .child(path.to_string()),
    )
    .into_any_element()
}
