//! Virtual scrolling widgets for efficiently displaying large content.
//!
//! This module provides two approaches for virtual scrolling:
//!
//! - [`show_viewport`] - For custom virtualization where you control what's rendered
//!   based on the visible viewport rectangle.
//! - [`show_rows`] - For simple uniform-height row virtualization.
//!
//! # Example: Virtual List with `show_rows`
//! ```no_run
//! # mod iced { pub mod widget { pub use icy_ui_widget::*; } }
//! # pub type Element<'a, Message> = icy_ui_widget::core::Element<'a, Message, icy_ui_widget::Theme, icy_ui_widget::Renderer>;
//! use icy_ui::widget::{column, text, virtual_scrollable};
//!
//! enum Message {}
//!
//! fn view(items: &[String]) -> Element<'_, Message> {
//!     virtual_scrollable::show_rows(
//!         30.0,  // row height
//!         items.len(),
//!         |visible_range| {
//!             column(
//!                 items[visible_range.clone()]
//!                     .iter()
//!                     .map(|item| text(item).into())
//!             ).into()
//!         }
//!     ).into()
//! }
//! ```
//!
//! # Example: Custom Virtualization with `show_viewport`
//! ```no_run
//! # mod iced { pub mod widget { pub use icy_ui_widget::*; } pub use icy_ui_widget::core::Size; }
//! # pub type Element<'a, Message> = icy_ui_widget::core::Element<'a, Message, icy_ui_widget::Theme, icy_ui_widget::Renderer>;
//! use icy_ui::widget::{column, text, virtual_scrollable};
//! use icy_ui::Size;
//!
//! enum Message {}
//!
//! fn view(items: &[String]) -> Element<'_, Message> {
//!     let row_height = 30.0;
//!     let total_height = items.len() as f32 * row_height;
//!
//!     virtual_scrollable::show_viewport(
//!         Size::new(400.0, total_height),
//!         |viewport| {
//!             let first = (viewport.y / row_height).floor() as usize;
//!             let last = ((viewport.y + viewport.height) / row_height).ceil() as usize;
//!             let visible = first..last.min(items.len());
//!
//!             column(
//!                 items[visible]
//!                     .iter()
//!                     .map(|item| text(item).into())
//!             ).into()
//!         }
//!     ).into()
//! }
//! ```
use crate::container;
use crate::core;
use crate::core::alignment;
use crate::core::border;
use crate::core::keyboard::{self, Key, key};
use crate::core::layout;
use crate::core::mouse;
use crate::core::overlay;
use crate::core::renderer;
use crate::core::text;
use crate::core::time::{Duration, Instant};
use crate::core::touch;
use crate::core::widget;
use crate::core::widget::operation::{self, Operation};
use crate::core::widget::tree::{self, Tree};
use crate::core::window;
use crate::core::{
    Animation, Background, Clipboard, Color, Element, Event, Layout, Length, Pixels, Point,
    Rectangle, Shell, Size, Vector, Widget,
};

use super::scrollable::{
    Anchor, Catalog, Direction, ScrollStyle, Scrollbar, Status, Style, StyleFn, Viewport,
};

pub use super::scrollable::{AbsoluteOffset, RelativeOffset};

use std::cell::RefCell;
use std::ops::Range;

/// Animation frame interval (~60fps) for smooth animations
const ANIMATION_FRAME_MS: u64 = 16;

/// Creates a virtual scrollable optimized for uniform-height rows.
///
/// This is a convenience wrapper around [`show_viewport`] for the common case
/// of a list with rows of equal height. The callback receives the range of
/// visible row indices.
///
/// # Arguments
/// * `row_height` - The height of each row in pixels
/// * `total_rows` - The total number of rows
/// * `view` - A callback that receives the visible row range and returns content
///
/// # Example
/// ```no_run
/// # mod iced { pub mod widget { pub use icy_ui_widget::*; } }
/// # pub type Element<'a, Message> = icy_ui_widget::core::Element<'a, Message, icy_ui_widget::Theme, icy_ui_widget::Renderer>;
/// use icy_ui::widget::{column, text, virtual_scrollable};
///
/// enum Message {}
///
/// let items: Vec<String> = (0..10_000).map(|i| format!("Item {}", i)).collect();
///
/// virtual_scrollable::show_rows(
///     30.0,  // each row is 30 pixels tall
///     items.len(),
///     |visible_range| {
///         column(
///             items[visible_range]
///                 .iter()
///                 .map(|item| text(item).into())
///         ).into()
///     }
/// );
/// ```
pub fn show_rows<'a, Message, Theme, Renderer>(
    row_height: f32,
    total_rows: usize,
    view: impl Fn(Range<usize>) -> Element<'a, Message, Theme, Renderer> + 'a,
) -> VirtualScrollable<'a, Message, Theme, Renderer>
where
    Theme: Catalog,
    Renderer: text::Renderer,
{
    VirtualScrollable::with_rows(row_height, total_rows, view)
}

/// A scrollable that only renders visible content for efficient large content display.
///
/// Use [`show_viewport`] or [`show_rows`] to create instances.
pub struct VirtualScrollable<'a, Message, Theme = crate::Theme, Renderer = crate::Renderer>
where
    Theme: Catalog,
    Renderer: text::Renderer,
{
    id: Option<widget::Id>,
    width: Length,
    height: Length,
    direction: Direction,
    content_size: Size,
    view: Box<dyn Fn(Rectangle) -> Element<'a, Message, Theme, Renderer> + 'a>,
    on_scroll: Option<Box<dyn Fn(Viewport) -> Message + 'a>>,
    auto_scroll: bool,
    class: Theme::Class<'a>,
    last_status: Option<Status>,
    /// Row height for smooth sub-row scrolling (internal, set by `with_rows`).
    /// When set, translation offset uses `translation % row_height` for smooth scrolling.
    row_height: Option<f32>,
    /// Optional cache key to invalidate the viewport cache when data changes.
    /// When this value changes, the view callback will be called even if the viewport hasn't changed.
    cache_key: u64,
    /// Cached content element and viewport to avoid rebuilding on every draw.
    /// The tuple is (viewport, element).
    cached_content: RefCell<Option<(Rectangle, Element<'a, Message, Theme, Renderer>)>>,
}

impl<'a, Message, Theme, Renderer> VirtualScrollable<'a, Message, Theme, Renderer>
where
    Theme: Catalog,
    Renderer: text::Renderer,
{
    /// Creates a new [`VirtualScrollable`] with the given content size and view function.
    pub fn new(
        content_size: Size,
        view: impl Fn(Rectangle) -> Element<'a, Message, Theme, Renderer> + 'a,
    ) -> Self {
        VirtualScrollable {
            id: None,
            width: Length::Fill,
            height: Length::Fill,
            direction: Direction::default(),
            content_size,
            view: Box::new(view),
            on_scroll: None,
            auto_scroll: false,
            class: Theme::default(),
            last_status: None,
            row_height: None,
            cache_key: 0,
            cached_content: RefCell::new(None),
        }
    }

    /// Creates a new [`VirtualScrollable`] optimized for uniform-height rows.
    ///
    /// This variant enables smooth sub-row scrolling by rendering one extra row
    /// and applying a fractional offset, similar to egui's `show_rows`.
    pub fn with_rows(
        row_height: f32,
        total_rows: usize,
        view: impl Fn(Range<usize>) -> Element<'a, Message, Theme, Renderer> + 'a,
    ) -> Self {
        let total_height = row_height * total_rows as f32;

        VirtualScrollable {
            id: None,
            width: Length::Fill,
            height: Length::Fill,
            direction: Direction::default(),
            content_size: Size::new(0.0, total_height), // Width will be determined by bounds
            view: Box::new(move |viewport| {
                let first_row = (viewport.y / row_height).floor().max(0.0) as usize;
                let last_row = ((viewport.y + viewport.height) / row_height).ceil() as usize + 1;
                let visible_range = first_row..last_row.min(total_rows);

                view(visible_range)
            }),
            on_scroll: None,
            auto_scroll: false,
            class: Theme::default(),
            last_status: None,
            row_height: Some(row_height),
            cache_key: 0,
            cached_content: RefCell::new(None),
        }
    }

    /// Sets whether the user should be allowed to auto-scroll with the middle mouse button.
    ///
    /// By default, it is disabled.
    pub fn auto_scroll(mut self, auto_scroll: bool) -> Self {
        self.auto_scroll = auto_scroll;
        self
    }

    /// Makes the [`VirtualScrollable`] scroll horizontally.
    pub fn horizontal(mut self) -> Self {
        self.direction = Direction::Horizontal(Scrollbar::default());
        self
    }

    /// Sets the [`Direction`] of the [`VirtualScrollable`].
    pub fn direction(mut self, direction: impl Into<Direction>) -> Self {
        self.direction = direction.into();
        self
    }

    /// Sets the [`widget::Id`] of the [`VirtualScrollable`].
    pub fn id(mut self, id: impl Into<widget::Id>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Sets the width of the [`VirtualScrollable`].
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the height of the [`VirtualScrollable`].
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Sets a function to call when the [`VirtualScrollable`] is scrolled.
    pub fn on_scroll(mut self, f: impl Fn(Viewport) -> Message + 'a) -> Self {
        self.on_scroll = Some(Box::new(f));
        self
    }

    /// Anchors the vertical [`VirtualScrollable`] direction to the top.
    pub fn anchor_top(self) -> Self {
        self.anchor_y(Anchor::Start)
    }

    /// Anchors the vertical [`VirtualScrollable`] direction to the bottom.
    pub fn anchor_bottom(self) -> Self {
        self.anchor_y(Anchor::End)
    }

    /// Anchors the horizontal [`VirtualScrollable`] direction to the left.
    pub fn anchor_left(self) -> Self {
        self.anchor_x(Anchor::Start)
    }

    /// Anchors the horizontal [`VirtualScrollable`] direction to the right.
    pub fn anchor_right(self) -> Self {
        self.anchor_x(Anchor::End)
    }

    /// Sets the [`Anchor`] of the horizontal direction.
    pub fn anchor_x(mut self, alignment: Anchor) -> Self {
        match &mut self.direction {
            Direction::Horizontal(horizontal) | Direction::Both { horizontal, .. } => {
                horizontal.alignment = alignment;
            }
            Direction::Vertical { .. } => {}
        }
        self
    }

    /// Sets the [`Anchor`] of the vertical direction.
    pub fn anchor_y(mut self, alignment: Anchor) -> Self {
        match &mut self.direction {
            Direction::Vertical(vertical) | Direction::Both { vertical, .. } => {
                vertical.alignment = alignment;
            }
            Direction::Horizontal { .. } => {}
        }
        self
    }

    /// Sets the style of this [`VirtualScrollable`].
    #[must_use]
    pub fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self
    where
        Theme::Class<'a>: From<StyleFn<'a, Theme>>,
    {
        self.class = (Box::new(style) as StyleFn<'a, Theme>).into();
        self
    }

    /// Sets the style class of the [`VirtualScrollable`].
    #[cfg(feature = "advanced")]
    #[must_use]
    pub fn class(mut self, class: impl Into<Theme::Class<'a>>) -> Self {
        self.class = class.into();
        self
    }

    /// Sets a cache key for the viewport content.
    ///
    /// The view callback is cached based on the visible viewport. If your data changes
    /// but the viewport stays the same, the cached content would be stale. Use this method
    /// to provide a key that changes when your data changes, forcing a refresh.
    ///
    /// Common patterns:
    /// - Use `items.len() as u64` if only the count matters
    /// - Use a hash of your data
    /// - Use a version counter that increments on data changes
    ///
    /// # Example
    /// ```ignore
    /// scroll_area()
    ///     .show_rows(30.0, items.len(), |range| { ... })
    ///     .cache_key(data_version)
    /// ```
    #[must_use]
    pub fn cache_key(mut self, key: u64) -> Self {
        self.cache_key = key;
        self
    }
}

#[derive(Debug, Clone)]
struct State {
    offset_y: Offset,
    offset_x: Offset,
    interaction: Interaction,
    last_notified: Option<Viewport>,
    last_scrolled: Option<Instant>,
    /// Animation for scrollbar hover state (0.0 = dormant, 1.0 = fully visible)
    hover_animation: Animation<bool>,
    /// Whether the mouse is currently over the scroll area
    is_mouse_over_area: bool,
    /// Current scroll velocity for kinetic scrolling (pixels per second)
    velocity: Vector<f32>,
    /// Last time kinetic scrolling was updated
    last_kinetic_update: Option<Instant>,
    /// Target offset for smooth scroll-to animation
    scroll_to_target: Option<AbsoluteOffset>,
    /// Animation for smooth scroll-to
    scroll_to_animation: Option<Animation<bool>>,
    /// Starting offset for smooth scroll-to animation
    scroll_to_start: Option<AbsoluteOffset>,

    is_y_scrollbar_visible: bool,
    is_x_scrollbar_visible: bool,

    /// Cached viewport for avoiding redundant view callback calls.
    /// When the visible viewport hasn't changed, we reuse the cached layout.
    cached_viewport: Option<Rectangle>,
    /// Cached layout node from the last view callback.
    cached_content_layout: Option<layout::Node>,
    /// Last cache key used to generate the cached content.
    cached_key: u64,

    /// Whether a pointer is currently pressed (mouse button down or touch active).
    /// Used to avoid rebuilding/updating viewport content on every hover move.
    is_pointer_down: bool,
}

#[derive(Debug, Clone, Copy)]
enum Interaction {
    None,
    YScrollerGrabbed(f32),
    XScrollerGrabbed(f32),
    AutoScrolling {
        origin: Point,
        current: Point,
        last_frame: Option<Instant>,
    },
    TouchScrolling {
        last_position: Point,
        last_time: Instant,
    },
}

impl Default for State {
    fn default() -> Self {
        Self {
            offset_y: Offset::Absolute(0.0),
            offset_x: Offset::Absolute(0.0),
            interaction: Interaction::None,
            last_notified: None,
            last_scrolled: None,
            hover_animation: Animation::new(false).quick(),
            is_mouse_over_area: false,
            velocity: Vector::new(0.0, 0.0),
            last_kinetic_update: None,
            scroll_to_target: None,
            scroll_to_animation: None,
            scroll_to_start: None,

            is_y_scrollbar_visible: true,
            is_x_scrollbar_visible: true,

            cached_viewport: None,
            cached_content_layout: None,
            cached_key: 0,

            is_pointer_down: false,
        }
    }
}

impl operation::Scrollable for State {
    fn snap_to(&mut self, offset: RelativeOffset<Option<f32>>) {
        State::snap_to(self, offset);
    }

    fn scroll_to(&mut self, offset: AbsoluteOffset<Option<f32>>) {
        State::scroll_to(self, offset);
    }

    fn scroll_by(&mut self, offset: AbsoluteOffset, bounds: Rectangle, content_bounds: Rectangle) {
        State::scroll_by(self, offset, bounds, content_bounds);
    }

    fn scroll_to_animated(
        &mut self,
        offset: AbsoluteOffset<Option<f32>>,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) {
        let target = AbsoluteOffset {
            x: offset
                .x
                .unwrap_or_else(|| self.offset_x.absolute(bounds.width, content_bounds.width)),
            y: offset
                .y
                .unwrap_or_else(|| self.offset_y.absolute(bounds.height, content_bounds.height)),
        };
        State::scroll_to_animated(self, target, bounds, content_bounds);
    }

    fn scroll_by_animated(
        &mut self,
        offset: AbsoluteOffset,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) {
        let current = AbsoluteOffset {
            x: self.offset_x.absolute(bounds.width, content_bounds.width),
            y: self.offset_y.absolute(bounds.height, content_bounds.height),
        };
        let target = AbsoluteOffset {
            x: current.x + offset.x,
            y: current.y + offset.y,
        };
        State::scroll_to_animated(self, target, bounds, content_bounds);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Offset {
    Absolute(f32),
    Relative(f32),
}

impl Offset {
    fn absolute(self, viewport: f32, content: f32) -> f32 {
        match self {
            Offset::Absolute(absolute) => absolute.min((content - viewport).max(0.0)),
            Offset::Relative(percentage) => ((content - viewport) * percentage).max(0.0),
        }
    }

    fn translation(self, viewport: f32, content: f32, alignment: Anchor) -> f32 {
        let offset = self.absolute(viewport, content);

        match alignment {
            Anchor::Start => offset,
            Anchor::End => ((content - viewport).max(0.0) - offset).max(0.0),
        }
    }
}

impl State {
    fn new() -> Self {
        State::default()
    }

    fn scroll(&mut self, delta: Vector<f32>, bounds: Rectangle, content_bounds: Rectangle) {
        if bounds.height < content_bounds.height {
            self.offset_y = Offset::Absolute(
                (self.offset_y.absolute(bounds.height, content_bounds.height) + delta.y)
                    .clamp(0.0, content_bounds.height - bounds.height),
            );
        }

        if bounds.width < content_bounds.width {
            self.offset_x = Offset::Absolute(
                (self.offset_x.absolute(bounds.width, content_bounds.width) + delta.x)
                    .clamp(0.0, content_bounds.width - bounds.width),
            );
        }
    }

    fn scroll_y_to(&mut self, percentage: f32, bounds: Rectangle, content_bounds: Rectangle) {
        self.offset_y = Offset::Relative(percentage.clamp(0.0, 1.0));
        self.unsnap(bounds, content_bounds);
    }

    fn scroll_x_to(&mut self, percentage: f32, bounds: Rectangle, content_bounds: Rectangle) {
        self.offset_x = Offset::Relative(percentage.clamp(0.0, 1.0));
        self.unsnap(bounds, content_bounds);
    }

    fn snap_to(&mut self, offset: RelativeOffset<Option<f32>>) {
        if let Some(x) = offset.x {
            self.offset_x = Offset::Relative(x.clamp(0.0, 1.0));
        }
        if let Some(y) = offset.y {
            self.offset_y = Offset::Relative(y.clamp(0.0, 1.0));
        }
    }

    fn scroll_to(&mut self, offset: AbsoluteOffset<Option<f32>>) {
        if let Some(x) = offset.x {
            self.offset_x = Offset::Absolute(x.max(0.0));
        }
        if let Some(y) = offset.y {
            self.offset_y = Offset::Absolute(y.max(0.0));
        }
    }

    fn scroll_by(&mut self, offset: AbsoluteOffset, bounds: Rectangle, content_bounds: Rectangle) {
        self.scroll(Vector::new(offset.x, offset.y), bounds, content_bounds);
    }

    fn unsnap(&mut self, bounds: Rectangle, content_bounds: Rectangle) {
        self.offset_x =
            Offset::Absolute(self.offset_x.absolute(bounds.width, content_bounds.width));
        self.offset_y =
            Offset::Absolute(self.offset_y.absolute(bounds.height, content_bounds.height));
    }

    fn translation(
        &self,
        direction: Direction,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) -> Vector {
        Vector::new(
            if let Some(horizontal) = direction.horizontal() {
                self.offset_x
                    .translation(bounds.width, content_bounds.width, horizontal.alignment)
                    .round()
            } else {
                0.0
            },
            if let Some(vertical) = direction.vertical() {
                self.offset_y
                    .translation(bounds.height, content_bounds.height, vertical.alignment)
                    .round()
            } else {
                0.0
            },
        )
    }

    /// Starts an animated scroll to the given absolute offset.
    fn scroll_to_animated(
        &mut self,
        target: AbsoluteOffset,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) {
        // Get current absolute offset
        let current = AbsoluteOffset {
            x: self.offset_x.absolute(bounds.width, content_bounds.width),
            y: self.offset_y.absolute(bounds.height, content_bounds.height),
        };

        // Clamp target to valid range
        let target = AbsoluteOffset {
            x: target
                .x
                .clamp(0.0, (content_bounds.width - bounds.width).max(0.0)),
            y: target
                .y
                .clamp(0.0, (content_bounds.height - bounds.height).max(0.0)),
        };

        self.scroll_to_start = Some(current);
        self.scroll_to_target = Some(target);
        self.scroll_to_animation = Some(Animation::new(false).slow().go(true, Instant::now()));

        // Stop any kinetic scrolling
        self.velocity = Vector::new(0.0, 0.0);
    }

    /// Updates kinetic scrolling, returning true if the scroll position changed.
    fn update_kinetic(
        &mut self,
        now: Instant,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) -> bool {
        const FRICTION: f32 = 5.0; // Deceleration factor
        const MIN_VELOCITY: f32 = 1.0; // Stop when velocity is below this (px/s)

        // Skip if velocity is negligible
        if self.velocity.x.abs() < MIN_VELOCITY && self.velocity.y.abs() < MIN_VELOCITY {
            self.velocity = Vector::new(0.0, 0.0);
            self.last_kinetic_update = None;
            return false;
        }

        let dt = if let Some(last) = self.last_kinetic_update {
            (now - last).as_secs_f32()
        } else {
            0.016 // ~60fps default
        };

        self.last_kinetic_update = Some(now);

        // Apply velocity to scroll position
        let delta = Vector::new(self.velocity.x * dt, self.velocity.y * dt);

        let old_x = self.offset_x;
        let old_y = self.offset_y;

        self.scroll(delta, bounds, content_bounds);

        // Apply friction (exponential decay)
        let decay = (-FRICTION * dt).exp();
        self.velocity.x *= decay;
        self.velocity.y *= decay;

        // Stop if we hit the edge
        if self.offset_x == old_x {
            self.velocity.x = 0.0;
        }
        if self.offset_y == old_y {
            self.velocity.y = 0.0;
        }

        true
    }

    /// Updates animated scroll-to, returning true if still animating.
    fn update_scroll_to_animation(
        &mut self,
        now: Instant,
        bounds: Rectangle,
        content_bounds: Rectangle,
    ) -> bool {
        let (Some(target), Some(start), Some(animation)) = (
            self.scroll_to_target,
            self.scroll_to_start,
            &self.scroll_to_animation,
        ) else {
            return false;
        };

        if !animation.is_animating(now) {
            // Animation complete, snap to target
            self.offset_x = Offset::Absolute(target.x);
            self.offset_y = Offset::Absolute(target.y);
            self.scroll_to_target = None;
            self.scroll_to_start = None;
            self.scroll_to_animation = None;
            return false;
        }

        // Interpolate using ease-out
        let t = animation.interpolate(0.0, 1.0, now);
        // Apply ease-out cubic for smoother deceleration
        let eased = 1.0 - (1.0 - t).powi(3);

        let current_x = start.x + (target.x - start.x) * eased;
        let current_y = start.y + (target.y - start.y) * eased;

        self.offset_x =
            Offset::Absolute(current_x.clamp(0.0, (content_bounds.width - bounds.width).max(0.0)));
        self.offset_y = Offset::Absolute(
            current_y.clamp(0.0, (content_bounds.height - bounds.height).max(0.0)),
        );

        true
    }

    /// Returns true if kinetic scrolling is active.
    fn is_kinetic_active(&self) -> bool {
        self.velocity.x.abs() > 1.0 || self.velocity.y.abs() > 1.0
    }

    /// Returns true if a scroll-to animation is active.
    fn is_scroll_to_animating(&self, now: Instant) -> bool {
        self.scroll_to_animation
            .as_ref()
            .is_some_and(|a| a.is_animating(now))
    }

    fn scrollers_grabbed(&self) -> bool {
        matches!(
            self.interaction,
            Interaction::YScrollerGrabbed(_) | Interaction::XScrollerGrabbed(_),
        )
    }

    /// Returns true if any animation is currently active that requires redraws.
    fn needs_animation(&self, now: Instant) -> bool {
        self.is_kinetic_active()
            || self.is_scroll_to_animating(now)
            || self.hover_animation.is_animating(now)
            || matches!(self.interaction, Interaction::AutoScrolling { .. })
    }

    /// Returns the next instant at which a redraw is needed for animations.
    /// Returns None if no animation is active.
    fn next_redraw_instant(&self, now: Instant) -> Option<Instant> {
        if self.needs_animation(now) {
            Some(now + Duration::from_millis(ANIMATION_FRAME_MS))
        } else {
            None
        }
    }

    fn y_scroller_grabbed_at(&self) -> Option<f32> {
        if let Interaction::YScrollerGrabbed(at) = self.interaction {
            Some(at)
        } else {
            None
        }
    }

    fn x_scroller_grabbed_at(&self) -> Option<f32> {
        if let Interaction::XScrollerGrabbed(at) = self.interaction {
            Some(at)
        } else {
            None
        }
    }
}

fn embedded_padding(direction: Direction, state: &State) -> (f32, f32) {
    let y_padding = direction
        .vertical()
        .and_then(|sb| {
            sb.spacing
                .map(|spacing| sb.width.max(sb.scroller_width) + sb.margin * 2.0 + spacing)
        })
        .unwrap_or(0.0);

    let x_padding = direction
        .horizontal()
        .and_then(|sb| {
            sb.spacing
                .map(|spacing| sb.width.max(sb.scroller_width) + sb.margin * 2.0 + spacing)
        })
        .unwrap_or(0.0);

    (
        if state.is_y_scrollbar_visible {
            y_padding
        } else {
            0.0
        },
        if state.is_x_scrollbar_visible {
            x_padding
        } else {
            0.0
        },
    )
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for VirtualScrollable<'_, Message, Theme, Renderer>
where
    Theme: Catalog,
    Renderer: text::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::new())
    }

    fn children(&self) -> Vec<Tree> {
        // We'll create the child dynamically based on viewport
        vec![]
    }

    fn diff(&self, _tree: &mut Tree) {
        // Children are rebuilt each frame based on viewport
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();

        // Calculate bounds from limits
        let bounds = limits.resolve(self.width, self.height, Size::ZERO);

        let (y_padding, x_padding) = match self.direction {
            Direction::Vertical(Scrollbar {
                width,
                margin,
                scroller_width,
                spacing: Some(spacing),
                ..
            }) => (
                Some(width.max(scroller_width) + margin * 2.0 + spacing),
                None,
            ),
            Direction::Horizontal(Scrollbar {
                width,
                margin,
                scroller_width,
                spacing: Some(spacing),
                ..
            }) => (
                None,
                Some(width.max(scroller_width) + margin * 2.0 + spacing),
            ),
            Direction::Both {
                vertical,
                horizontal,
            } => (
                vertical.spacing.map(|spacing| {
                    vertical.width.max(vertical.scroller_width) + vertical.margin * 2.0 + spacing
                }),
                horizontal.spacing.map(|spacing| {
                    horizontal.width.max(horizontal.scroller_width)
                        + horizontal.margin * 2.0
                        + spacing
                }),
            ),
            _ => (None, None),
        };

        let y_embedded = y_padding.is_some();
        let x_embedded = x_padding.is_some();

        let y_padding = y_padding.unwrap_or(0.0);
        let x_padding = x_padding.unwrap_or(0.0);

        let mut is_y_scrollbar_visible = state.is_y_scrollbar_visible && y_embedded;
        let mut is_x_scrollbar_visible = state.is_x_scrollbar_visible && x_embedded;

        if self.direction.vertical().is_none() {
            is_y_scrollbar_visible = false;
        }

        if self.direction.horizontal().is_none() {
            is_x_scrollbar_visible = false;
        }

        // Determine content bounds based on declared content_size
        let content_width = if self.content_size.width > 0.0 {
            self.content_size.width
        } else {
            bounds.width
        };
        let content_height = if self.content_size.height > 0.0 {
            self.content_size.height
        } else {
            bounds.height
        };

        // Resolve scrollbar visibility with a short convergence loop (Both can be interdependent).
        for _ in 0..2 {
            let viewport_width = (bounds.width
                - if is_y_scrollbar_visible {
                    y_padding
                } else {
                    0.0
                })
            .max(0.0);

            let viewport_height = (bounds.height
                - if is_x_scrollbar_visible {
                    x_padding
                } else {
                    0.0
                })
            .max(0.0);

            let y_needed = y_embedded && content_height > viewport_height;
            let x_needed = x_embedded && content_width > viewport_width;

            if y_needed == is_y_scrollbar_visible && x_needed == is_x_scrollbar_visible {
                break;
            }

            is_y_scrollbar_visible = y_needed;
            is_x_scrollbar_visible = x_needed;
        }

        state.is_y_scrollbar_visible = is_y_scrollbar_visible;
        state.is_x_scrollbar_visible = is_x_scrollbar_visible;

        let viewport_width = (bounds.width
            - if is_y_scrollbar_visible {
                y_padding
            } else {
                0.0
            })
        .max(0.0);
        let viewport_height = (bounds.height
            - if is_x_scrollbar_visible {
                x_padding
            } else {
                0.0
            })
        .max(0.0);

        let content_bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: content_width,
            height: content_height,
        };

        // Calculate visible viewport in content coordinates
        // Like egui: the viewport is simply the visible area offset by the scroll position
        let translation = state.translation(
            self.direction,
            Rectangle::with_size(Size::new(viewport_width, viewport_height)),
            content_bounds,
        );

        let visible_viewport = Rectangle {
            x: translation.x,
            y: translation.y,
            width: viewport_width
                .min(content_bounds.width - translation.x)
                .max(0.0),
            height: viewport_height
                .min(content_bounds.height - translation.y)
                .max(0.0),
        };

        // Check if we can reuse the cached layout
        let viewport_changed = state
            .cached_viewport
            .map(|cached| {
                // Consider viewport changed if position or size differs
                // Use a small epsilon for floating point comparison
                const EPSILON: f32 = 0.01;
                (cached.x - visible_viewport.x).abs() > EPSILON
                    || (cached.y - visible_viewport.y).abs() > EPSILON
                    || (cached.width - visible_viewport.width).abs() > EPSILON
                    || (cached.height - visible_viewport.height).abs() > EPSILON
            })
            .unwrap_or(true);

        // Also check if the cache key changed (data invalidation)
        let cache_key_changed = state.cached_key != self.cache_key;

        let content_node =
            if viewport_changed || cache_key_changed || state.cached_content_layout.is_none() {
                // Viewport changed, cache key changed, or no cache - call the view callback
                let mut content = (self.view)(visible_viewport);

                // Create a temporary tree for the content
                if tree.children.is_empty() {
                    tree.children.push(Tree::new(content.as_widget()));
                } else {
                    tree.children[0].diff(content.as_widget());
                }

                // Layout the visible content within the visible bounds
                let content_limits =
                    layout::Limits::new(Size::ZERO, Size::new(viewport_width, viewport_height));
                let content_node = content.as_widget_mut().layout(
                    &mut tree.children[0],
                    renderer,
                    &content_limits,
                );

                // Cache the viewport, key, and layout for next time
                state.cached_viewport = Some(visible_viewport);
                state.cached_content_layout = Some(content_node.clone());
                state.cached_key = self.cache_key;

                // Also cache the content element for draw() to reuse
                *self.cached_content.borrow_mut() = Some((visible_viewport, content));

                content_node
            } else {
                // Viewport and cache key unchanged - reuse cached layout
                state.cached_content_layout.clone().unwrap()
            };

        // The main node has the outer bounds, with content as child (shrunken by right/bottom padding)
        layout::Node::with_children(bounds, vec![content_node])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();

        let (right_padding, bottom_padding) = embedded_padding(self.direction, state);
        let viewport_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: (bounds.width - right_padding).max(0.0),
            height: (bounds.height - bottom_padding).max(0.0),
        };

        let content_bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: if self.content_size.width > 0.0 {
                self.content_size.width
            } else {
                bounds.width
            },
            height: if self.content_size.height > 0.0 {
                self.content_size.height
            } else {
                bounds.height
            },
        };

        let translation = state.translation(self.direction, viewport_bounds, content_bounds);

        operation.scrollable(self.id.as_ref(), bounds, content_bounds, translation, state);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        const AUTOSCROLL_DEADZONE: f32 = 20.0;
        const AUTOSCROLL_SMOOTHNESS: f32 = 1.5;

        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();

        // Track pressed state so we can keep drag interactions working while
        // avoiding expensive content rebuilds on hover-only cursor movement.
        match event {
            Event::Mouse(mouse::Event::ButtonPressed { .. })
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                state.is_pointer_down = true;
            }
            Event::Mouse(mouse::Event::ButtonReleased { .. })
            | Event::Touch(touch::Event::FingerLifted { .. } | touch::Event::FingerLost { .. }) => {
                state.is_pointer_down = false;
            }
            _ => {}
        }
        let cursor_over_scrollable = cursor.position_over(bounds);

        let (right_padding, bottom_padding) = embedded_padding(self.direction, state);
        let viewport = Size::new(
            (bounds.width - right_padding).max(0.0),
            (bounds.height - bottom_padding).max(0.0),
        );

        let viewport_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: viewport.width,
            height: viewport.height,
        };

        let content_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: if self.content_size.width > 0.0 {
                self.content_size.width
            } else {
                bounds.width
            },
            height: if self.content_size.height > 0.0 {
                self.content_size.height
            } else {
                bounds.height
            },
        };

        let scrollbars = Scrollbars::new(state, self.direction, bounds, viewport, content_bounds);
        let (mouse_over_y_scrollbar, mouse_over_x_scrollbar) = scrollbars.is_mouse_over(cursor);

        let last_offsets = (state.offset_x, state.offset_y);

        if let Some(last_scrolled) = state.last_scrolled {
            let clear_transaction = match event {
                Event::Mouse(
                    mouse::Event::ButtonPressed { .. }
                    | mouse::Event::ButtonReleased { .. }
                    | mouse::Event::CursorLeft,
                ) => true,
                Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                    last_scrolled.elapsed() > Duration::from_millis(100)
                }
                _ => last_scrolled.elapsed() > Duration::from_millis(1500),
            };

            if clear_transaction {
                state.last_scrolled = None;
            }
        }

        if matches!(state.interaction, Interaction::AutoScrolling { .. })
            && matches!(
                event,
                Event::Mouse(
                    mouse::Event::ButtonPressed { .. } | mouse::Event::WheelScrolled { .. }
                ) | Event::Touch(_)
                    | Event::Keyboard(_)
            )
        {
            state.interaction = Interaction::None;
            shell.capture_event();
            shell.request_redraw();
            return;
        }

        let mut update = || {
            // Handle Y scrollbar dragging
            if let Some(scroller_grabbed_at) = state.y_scroller_grabbed_at() {
                match event {
                    Event::Mouse(mouse::Event::CursorMoved { .. })
                    | Event::Touch(touch::Event::FingerMoved { .. }) => {
                        if let Some(scrollbar) = scrollbars.y {
                            let Some(cursor_position) = cursor.land().position() else {
                                return;
                            };

                            state.scroll_y_to(
                                scrollbar.scroll_percentage_y(scroller_grabbed_at, cursor_position),
                                viewport_bounds,
                                content_bounds,
                            );

                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                bounds,
                                content_bounds,
                                shell,
                            );

                            shell.capture_event();
                        }
                    }
                    _ => {}
                }
            } else if mouse_over_y_scrollbar {
                match event {
                    Event::Mouse(mouse::Event::ButtonPressed {
                        button: mouse::Button::Left,
                        ..
                    })
                    | Event::Touch(touch::Event::FingerPressed { .. }) => {
                        let Some(cursor_position) = cursor.position() else {
                            return;
                        };

                        if let (Some(scroller_grabbed_at), Some(scrollbar)) =
                            (scrollbars.grab_y_scroller(cursor_position), scrollbars.y)
                        {
                            state.scroll_y_to(
                                scrollbar.scroll_percentage_y(scroller_grabbed_at, cursor_position),
                                bounds,
                                content_bounds,
                            );

                            state.interaction = Interaction::YScrollerGrabbed(scroller_grabbed_at);

                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                bounds,
                                content_bounds,
                                shell,
                            );
                        }

                        shell.capture_event();
                    }
                    _ => {}
                }
            }

            // Handle X scrollbar dragging
            if let Some(scroller_grabbed_at) = state.x_scroller_grabbed_at() {
                match event {
                    Event::Mouse(mouse::Event::CursorMoved { .. })
                    | Event::Touch(touch::Event::FingerMoved { .. }) => {
                        let Some(cursor_position) = cursor.land().position() else {
                            return;
                        };

                        if let Some(scrollbar) = scrollbars.x {
                            state.scroll_x_to(
                                scrollbar.scroll_percentage_x(scroller_grabbed_at, cursor_position),
                                viewport_bounds,
                                content_bounds,
                            );

                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                bounds,
                                content_bounds,
                                shell,
                            );
                        }

                        shell.capture_event();
                    }
                    _ => {}
                }
            } else if mouse_over_x_scrollbar {
                match event {
                    Event::Mouse(mouse::Event::ButtonPressed {
                        button: mouse::Button::Left,
                        ..
                    })
                    | Event::Touch(touch::Event::FingerPressed { .. }) => {
                        let Some(cursor_position) = cursor.position() else {
                            return;
                        };

                        if let (Some(scroller_grabbed_at), Some(scrollbar)) =
                            (scrollbars.grab_x_scroller(cursor_position), scrollbars.x)
                        {
                            state.scroll_x_to(
                                scrollbar.scroll_percentage_x(scroller_grabbed_at, cursor_position),
                                bounds,
                                content_bounds,
                            );

                            state.interaction = Interaction::XScrollerGrabbed(scroller_grabbed_at);

                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                bounds,
                                content_bounds,
                                shell,
                            );

                            shell.capture_event();
                        }
                    }
                    _ => {}
                }
            }

            // Forward events to content if not interacting with scrollbars
            let mut forward_to_content = state.last_scrolled.is_none()
                || !matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }));

            if forward_to_content {
                // Window-level events (e.g. RedrawRequested) are handled internally by
                // VirtualScrollable and don't need to be forwarded to content.
                if matches!(event, Event::Window(_)) {
                    forward_to_content = false;
                }
            }

            if forward_to_content {
                let translation = state.translation(self.direction, bounds, content_bounds);

                // Content is rendered without translation. Give it viewport-local coordinates.
                let cursor = match cursor_over_scrollable {
                    Some(cursor_position)
                        if !(mouse_over_x_scrollbar || mouse_over_y_scrollbar) =>
                    {
                        mouse::Cursor::Available(cursor_position)
                    }
                    _ => cursor.levitate(),
                };

                // Calculate visible viewport (using viewport size, not bounds, to match layout())
                let visible_viewport = Rectangle {
                    x: translation.x,
                    y: translation.y,
                    width: viewport_bounds
                        .width
                        .min(content_bounds.width - translation.x)
                        .max(0.0),
                    height: viewport_bounds
                        .height
                        .min(content_bounds.height - translation.y)
                        .max(0.0),
                };

                // Try to use cached content instead of regenerating
                let mut cached = self.cached_content.borrow_mut();
                let content_from_cache = cached
                    .as_ref()
                    .map(|(cached_vp, _)| {
                        const EPSILON: f32 = 0.01;
                        (cached_vp.x - visible_viewport.x).abs() <= EPSILON
                            && (cached_vp.y - visible_viewport.y).abs() <= EPSILON
                            && (cached_vp.width - visible_viewport.width).abs() <= EPSILON
                            && (cached_vp.height - visible_viewport.height).abs() <= EPSILON
                    })
                    .unwrap_or(false);

                if content_from_cache {
                    // Use cached content for update
                    let (_, content) = cached.as_mut().unwrap();

                    if tree.children.is_empty() {
                        tree.children.push(Tree::new(content.as_widget()));
                    }

                    if let Some(content_layout) = layout.children().next() {
                        content.as_widget_mut().update(
                            &mut tree.children[0],
                            event,
                            content_layout,
                            cursor,
                            renderer,
                            clipboard,
                            shell,
                            &bounds,
                        );
                    }
                } else {
                    // Cache miss - regenerate content
                    drop(cached);

                    let mut content = (self.view)(visible_viewport);

                    if tree.children.is_empty() {
                        tree.children.push(Tree::new(content.as_widget()));
                    } else {
                        tree.children[0].diff(content.as_widget());
                    }

                    if let Some(content_layout) = layout.children().next() {
                        content.as_widget_mut().update(
                            &mut tree.children[0],
                            event,
                            content_layout,
                            cursor,
                            renderer,
                            clipboard,
                            shell,
                            &bounds,
                        );
                    }

                    // Update cache
                    *self.cached_content.borrow_mut() = Some((visible_viewport, content));
                }
            }

            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased {
                    button: mouse::Button::Left,
                    ..
                }) | Event::Touch(
                    touch::Event::FingerLifted { .. } | touch::Event::FingerLost { .. }
                )
            ) {
                // Start kinetic scrolling if we were touch scrolling
                if matches!(state.interaction, Interaction::TouchScrolling { .. }) {
                    // Apply direction constraints to velocity
                    state.velocity = self.direction.align(state.velocity);
                    state.last_kinetic_update = Some(Instant::now());
                    // Schedule redraw to animate kinetic scrolling
                    if state.is_kinetic_active() {
                        let now = Instant::now();
                        shell.request_redraw_at(now + Duration::from_millis(ANIMATION_FRAME_MS));
                    }
                }
                state.interaction = Interaction::None;
                return;
            }

            if shell.is_event_captured() {
                return;
            }

            match event {
                Event::Mouse(mouse::Event::WheelScrolled { delta, modifiers }) => {
                    if cursor_over_scrollable.is_none() {
                        return;
                    }

                    // Stop any kinetic scrolling
                    state.velocity = Vector::new(0.0, 0.0);
                    state.last_kinetic_update = None;
                    // Cancel any animated scroll
                    state.scroll_to_target = None;
                    state.scroll_to_animation = None;

                    let delta = match *delta {
                        mouse::ScrollDelta::Lines { x, y } => {
                            let is_shift_pressed = modifiers.shift();

                            let (x, y) = if cfg!(target_os = "macos") && is_shift_pressed {
                                (y, x)
                            } else {
                                (x, y)
                            };

                            let movement = if !is_shift_pressed {
                                Vector::new(x, y)
                            } else {
                                Vector::new(y, x)
                            };

                            -movement * 60.0
                        }
                        mouse::ScrollDelta::Pixels { x, y } => -Vector::new(x, y),
                    };

                    state.scroll(self.direction.align(delta), viewport_bounds, content_bounds);

                    let has_scrolled = notify_scroll(
                        state,
                        &self.on_scroll,
                        viewport_bounds,
                        content_bounds,
                        shell,
                    );

                    let in_transaction = state.last_scrolled.is_some();

                    if has_scrolled || in_transaction {
                        shell.capture_event();
                    }
                }
                Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    // Match existing widget behavior (e.g. slider): allow keyboard interaction
                    // while hovering, since VirtualScrollable does not manage focus.
                    if cursor_over_scrollable.is_none() {
                        return;
                    }

                    let has_vertical = self.direction.vertical().is_some();
                    let has_horizontal = self.direction.horizontal().is_some();

                    if !has_vertical && !has_horizontal {
                        return;
                    }

                    let current = AbsoluteOffset {
                        x: state
                            .offset_x
                            .absolute(viewport_bounds.width, content_bounds.width),
                        y: state
                            .offset_y
                            .absolute(viewport_bounds.height, content_bounds.height),
                    };

                    let page_x = viewport_bounds.width;
                    let page_y = viewport_bounds.height;

                    let max_x = (content_bounds.width - viewport_bounds.width).max(0.0);
                    let max_y = (content_bounds.height - viewport_bounds.height).max(0.0);

                    let mut target = current;

                    match key {
                        Key::Named(key::Named::PageUp) => {
                            if modifiers.shift() && has_horizontal {
                                target.x = (target.x - page_x).clamp(0.0, max_x);
                            } else if has_vertical {
                                target.y = (target.y - page_y).clamp(0.0, max_y);
                            } else if has_horizontal {
                                target.x = (target.x - page_x).clamp(0.0, max_x);
                            }
                        }
                        Key::Named(key::Named::PageDown) => {
                            if modifiers.shift() && has_horizontal {
                                target.x = (target.x + page_x).clamp(0.0, max_x);
                            } else if has_vertical {
                                target.y = (target.y + page_y).clamp(0.0, max_y);
                            } else if has_horizontal {
                                target.x = (target.x + page_x).clamp(0.0, max_x);
                            }
                        }
                        Key::Named(key::Named::Home) => {
                            if has_vertical {
                                target.y = 0.0;
                            }
                            if modifiers.shift() && has_horizontal {
                                target.x = 0.0;
                            }
                        }
                        Key::Named(key::Named::End) => {
                            if has_vertical {
                                target.y = max_y;
                            }
                            if modifiers.shift() && has_horizontal {
                                target.x = max_x;
                            }
                        }
                        _ => {
                            return;
                        }
                    }

                    // Cancel any ongoing kinetic scrolling and start an animated scroll.
                    state.velocity = Vector::new(0.0, 0.0);
                    state.last_kinetic_update = None;

                    state.scroll_to_animated(target, viewport_bounds, content_bounds);
                    shell.capture_event();
                    let now = Instant::now();
                    shell.request_redraw_at(now + Duration::from_millis(ANIMATION_FRAME_MS));
                }
                Event::Mouse(mouse::Event::ButtonPressed {
                    button: mouse::Button::Middle,
                    ..
                }) if self.auto_scroll && matches!(state.interaction, Interaction::None) => {
                    let Some(origin) = cursor_over_scrollable else {
                        return;
                    };

                    state.interaction = Interaction::AutoScrolling {
                        origin,
                        current: origin,
                        last_frame: None,
                    };

                    shell.capture_event();
                    let now = Instant::now();
                    shell.request_redraw_at(now + Duration::from_millis(ANIMATION_FRAME_MS));
                }
                Event::Touch(event)
                    if matches!(state.interaction, Interaction::TouchScrolling { .. })
                        || (!mouse_over_y_scrollbar && !mouse_over_x_scrollbar) =>
                {
                    match event {
                        touch::Event::FingerPressed { .. } => {
                            let Some(position) = cursor_over_scrollable else {
                                return;
                            };

                            // Stop any kinetic scrolling
                            state.velocity = Vector::new(0.0, 0.0);
                            state.last_kinetic_update = None;
                            // Cancel any animated scroll
                            state.scroll_to_target = None;
                            state.scroll_to_animation = None;

                            state.interaction = Interaction::TouchScrolling {
                                last_position: position,
                                last_time: Instant::now(),
                            };
                        }
                        touch::Event::FingerMoved { .. } => {
                            let Interaction::TouchScrolling {
                                last_position,
                                last_time,
                            } = state.interaction
                            else {
                                return;
                            };

                            let Some(cursor_position) = cursor.position() else {
                                return;
                            };

                            let now = Instant::now();
                            let dt = (now - last_time).as_secs_f32().max(0.001);

                            let delta = Vector::new(
                                last_position.x - cursor_position.x,
                                last_position.y - cursor_position.y,
                            );

                            // Update velocity with exponential smoothing
                            let instant_velocity = Vector::new(-delta.x / dt, -delta.y / dt);
                            let smoothing = 0.3;
                            state.velocity.x = state.velocity.x * (1.0 - smoothing)
                                + instant_velocity.x * smoothing;
                            state.velocity.y = state.velocity.y * (1.0 - smoothing)
                                + instant_velocity.y * smoothing;

                            state.scroll(
                                self.direction.align(delta),
                                viewport_bounds,
                                content_bounds,
                            );
                            state.interaction = Interaction::TouchScrolling {
                                last_position: cursor_position,
                                last_time: now,
                            };

                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                viewport_bounds,
                                content_bounds,
                                shell,
                            );
                        }
                        _ => {}
                    }

                    shell.capture_event();
                }
                Event::Mouse(mouse::Event::CursorMoved { position, .. }) => {
                    if let Interaction::AutoScrolling {
                        origin, last_frame, ..
                    } = state.interaction
                    {
                        let delta = *position - origin;

                        state.interaction = Interaction::AutoScrolling {
                            origin,
                            current: *position,
                            last_frame,
                        };

                        if (delta.x.abs() >= AUTOSCROLL_DEADZONE
                            || delta.y.abs() >= AUTOSCROLL_DEADZONE)
                            && last_frame.is_none()
                        {
                            let now = Instant::now();
                            shell
                                .request_redraw_at(now + Duration::from_millis(ANIMATION_FRAME_MS));
                        }
                    }
                }
                Event::Window(window::Event::RedrawRequested(now)) => {
                    // Update hover animation state (consolidated here instead of outside update())
                    let is_mouse_over = cursor_over_scrollable.is_some();
                    if is_mouse_over != state.is_mouse_over_area {
                        state.is_mouse_over_area = is_mouse_over;
                        state.hover_animation.go_mut(is_mouse_over, *now);
                    }

                    if let Interaction::AutoScrolling {
                        origin,
                        current,
                        last_frame,
                    } = state.interaction
                    {
                        if last_frame == Some(*now) {
                            // Schedule next frame
                            if let Some(next) = state.next_redraw_instant(*now) {
                                shell.request_redraw_at(next);
                            }
                            return;
                        }

                        state.interaction = Interaction::AutoScrolling {
                            origin,
                            current,
                            last_frame: None,
                        };

                        let mut delta = current - origin;

                        if delta.x.abs() < AUTOSCROLL_DEADZONE {
                            delta.x = 0.0;
                        }

                        if delta.y.abs() < AUTOSCROLL_DEADZONE {
                            delta.y = 0.0;
                        }

                        if delta.x != 0.0 || delta.y != 0.0 {
                            let time_delta = if let Some(last_frame) = last_frame {
                                *now - last_frame
                            } else {
                                Duration::ZERO
                            };

                            let scroll_factor = time_delta.as_secs_f32();

                            state.scroll(
                                self.direction.align(Vector::new(
                                    delta.x.signum()
                                        * delta.x.abs().powf(AUTOSCROLL_SMOOTHNESS)
                                        * scroll_factor,
                                    delta.y.signum()
                                        * delta.y.abs().powf(AUTOSCROLL_SMOOTHNESS)
                                        * scroll_factor,
                                )),
                                viewport_bounds,
                                content_bounds,
                            );

                            let has_scrolled = notify_scroll(
                                state,
                                &self.on_scroll,
                                viewport_bounds,
                                content_bounds,
                                shell,
                            );

                            if has_scrolled || time_delta.is_zero() {
                                state.interaction = Interaction::AutoScrolling {
                                    origin,
                                    current,
                                    last_frame: Some(*now),
                                };
                            }
                        }
                    }

                    // Update kinetic scrolling
                    if state.is_kinetic_active() {
                        if state.update_kinetic(*now, viewport_bounds, content_bounds) {
                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                viewport_bounds,
                                content_bounds,
                                shell,
                            );
                        }
                    }

                    // Update scroll-to animation
                    if state.is_scroll_to_animating(*now) {
                        if state.update_scroll_to_animation(*now, viewport_bounds, content_bounds) {
                            let _ = notify_scroll(
                                state,
                                &self.on_scroll,
                                viewport_bounds,
                                content_bounds,
                                shell,
                            );
                        }
                    }

                    let _ = notify_viewport(
                        state,
                        &self.on_scroll,
                        viewport_bounds,
                        content_bounds,
                        shell,
                    );

                    // Schedule next redraw only if animations are still active
                    if let Some(next) = state.next_redraw_instant(*now) {
                        shell.request_redraw_at(next);
                    }
                }
                _ => {}
            }
        };

        update();

        // For non-RedrawRequested events, update hover animation state if needed
        let now = Instant::now();
        let is_mouse_over = cursor_over_scrollable.is_some();
        if !matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            if is_mouse_over != state.is_mouse_over_area {
                state.is_mouse_over_area = is_mouse_over;
                state.hover_animation.go_mut(is_mouse_over, now);
                // Schedule redraw for the animation
                if let Some(next) = state.next_redraw_instant(now) {
                    shell.request_redraw_at(next);
                }
            }
        }

        // Calculate hover factor from animation
        let hover_factor = state.hover_animation.interpolate(0.0, 1.0, now);

        let status = if state.scrollers_grabbed() {
            Status::Dragged {
                hover_factor,
                is_horizontal_scrollbar_dragged: state.x_scroller_grabbed_at().is_some(),
                is_vertical_scrollbar_dragged: state.y_scroller_grabbed_at().is_some(),
                is_horizontal_scrollbar_disabled: scrollbars.is_x_disabled(),
                is_vertical_scrollbar_disabled: scrollbars.is_y_disabled(),
            }
        } else if cursor_over_scrollable.is_some() {
            Status::Hovered {
                hover_factor,
                is_horizontal_scrollbar_hovered: mouse_over_x_scrollbar,
                is_vertical_scrollbar_hovered: mouse_over_y_scrollbar,
                is_horizontal_scrollbar_disabled: scrollbars.is_x_disabled(),
                is_vertical_scrollbar_disabled: scrollbars.is_y_disabled(),
            }
        } else {
            Status::Active {
                hover_factor,
                is_horizontal_scrollbar_disabled: scrollbars.is_x_disabled(),
                is_vertical_scrollbar_disabled: scrollbars.is_y_disabled(),
            }
        };

        if let Event::Window(window::Event::RedrawRequested(_now)) = event {
            self.last_status = Some(status);
        }

        // Only request immediate redraw if scroll offset changed (not for status/animation changes)
        if last_offsets != (state.offset_x, state.offset_y) {
            shell.request_redraw();
        } else if self.last_status.is_some_and(|last_status| {
            // Compare status structurally, ignoring hover_factor to avoid animation-induced redraw loops
            std::mem::discriminant(&last_status) != std::mem::discriminant(&status)
                || status_fields_changed(last_status, status)
        }) {
            // Status changed structurally (e.g., Active -> Hovered) - schedule next animation frame
            if let Some(next) = state.next_redraw_instant(now) {
                shell.request_redraw_at(next);
            }
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();

        let (right_padding, bottom_padding) = embedded_padding(self.direction, state);
        let viewport_size = Size::new(
            (bounds.width - right_padding).max(0.0),
            (bounds.height - bottom_padding).max(0.0),
        );

        let Some(visible_bounds) = bounds.intersection(viewport) else {
            return;
        };

        let content_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: if self.content_size.width > 0.0 {
                self.content_size.width
            } else {
                bounds.width
            },
            height: if self.content_size.height > 0.0 {
                self.content_size.height
            } else {
                bounds.height
            },
        };

        let scrollbars =
            Scrollbars::new(state, self.direction, bounds, viewport_size, content_bounds);
        let cursor_over_scrollable = cursor.position_over(bounds);
        let (mouse_over_y_scrollbar, mouse_over_x_scrollbar) = scrollbars.is_mouse_over(cursor);

        let translation = state.translation(
            self.direction,
            Rectangle {
                x: bounds.x,
                y: bounds.y,
                width: viewport_size.width,
                height: viewport_size.height,
            },
            content_bounds,
        );

        // Virtual scrolling: we render content for the visible viewport, but we do NOT
        // translate the rendered content. Therefore, the content receives viewport-local
        // cursor coordinates.
        let cursor = match cursor_over_scrollable {
            Some(cursor_position) if !(mouse_over_x_scrollbar || mouse_over_y_scrollbar) => {
                mouse::Cursor::Available(cursor_position)
            }
            _ => mouse::Cursor::Unavailable,
        };

        let status = self.last_status.unwrap_or(Status::Active {
            hover_factor: 0.0,
            is_horizontal_scrollbar_disabled: false,
            is_vertical_scrollbar_disabled: false,
        });

        let style = theme.style(&self.class, status);

        container::draw_background(renderer, &style.container, layout.bounds());

        // Draw virtual content
        if scrollbars.active() {

            // Use cached content from layout(). The tree was diffed against
            // this cached widget, so it is safe to draw with `&Tree`. Even if
            // the viewport drifted slightly, drawing the cached content avoids
            // flicker; the next layout pass will refresh it.
            let cached = self.cached_content.borrow();
            if let Some((_, content)) = cached.as_ref() {
                let Some(content_tree) = tree.children.first() else {
                    return;
                };

                renderer.with_layer(visible_bounds, |renderer| {
                    if let Some(content_layout) = layout.children().next() {
                        // Calculate translation offset for smooth scrolling
                        let (offset_x, offset_y) = if let Some(row_height) = self.row_height {
                            let offset_y = translation.y % row_height;
                            let offset_x = translation.x - translation.x.floor();
                            (offset_x, offset_y)
                        } else {
                            (
                                translation.x - translation.x.floor(),
                                translation.y - translation.y.floor(),
                            )
                        };

                        renderer.with_translation(Vector::new(-offset_x, -offset_y), |renderer| {
                            content.as_widget().draw(
                                content_tree,
                                renderer,
                                theme,
                                defaults,
                                content_layout,
                                cursor,
                                &visible_bounds,
                            );
                        });
                    }
                });
            }

            // Draw scrollbars
            let scroll_style = &style.scroll;
            let corner_radius = crate::core::border::rounded(scroll_style.corner_radius as u32);

            let draw_scrollbar = |renderer: &mut Renderer,
                                  scroll_style: &ScrollStyle,
                                  scrollbar: &internals::Scrollbar,
                                  is_vertical: bool,
                                  is_hovered: bool,
                                  is_dragged: bool| {
                let is_interacting = is_hovered || is_dragged;

                let shrink_axis = |bounds: Rectangle, target_thickness: f32| -> Rectangle {
                    let target_thickness = target_thickness.max(0.0).min(if is_vertical {
                        bounds.width
                    } else {
                        bounds.height
                    });

                    if is_vertical {
                        Rectangle {
                            // Flush to the outer edge (right)
                            x: bounds.x + bounds.width - target_thickness,
                            width: target_thickness,
                            ..bounds
                        }
                    } else {
                        Rectangle {
                            // Flush to the outer edge (bottom)
                            y: bounds.y + bounds.height - target_thickness,
                            height: target_thickness,
                            ..bounds
                        }
                    }
                };

                // For the `thin` preset we want a collapsed thin line unless the user is
                // hovering/dragging the scrollbar itself.
                //
                // We distinguish it from regular `floating` by the fact that `thin` allocates
                // a small width (`floating_allocated_width > 0`), while `floating` allocates none.
                let is_thin_like =
                    scroll_style.floating && scroll_style.floating_allocated_width > 0.0;

                let scrollbar_bounds = if is_thin_like && !is_interacting {
                    shrink_axis(scrollbar.bounds, scroll_style.floating_width)
                } else {
                    scrollbar.bounds
                };

                let scroller_bounds = if is_thin_like && !is_interacting {
                    scrollbar
                        .scroller
                        .map(|s| shrink_axis(s.bounds, scroll_style.floating_width))
                } else {
                    scrollbar.scroller.map(|s| s.bounds)
                };

                // Draw rail background
                if scrollbar_bounds.width > 0.0
                    && scrollbar_bounds.height > 0.0
                    && scroll_style.rail_background.is_some()
                {
                    let bg_color = scroll_style.rail_background.unwrap_or(Color::TRANSPARENT);
                    if bg_color != Color::TRANSPARENT {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: scrollbar_bounds,
                                border: corner_radius,
                                ..renderer::Quad::default()
                            },
                            Background::Color(bg_color),
                        );
                    }
                }

                // Draw handle/scroller
                if let Some(bounds) = scroller_bounds
                    && bounds.width > 0.0
                    && bounds.height > 0.0
                {
                    let handle_color = if is_dragged {
                        scroll_style.handle_color_dragged
                    } else if is_hovered {
                        scroll_style.handle_color_hovered
                    } else {
                        scroll_style.handle_color
                    };

                    if handle_color != Color::TRANSPARENT {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                border: corner_radius,
                                ..renderer::Quad::default()
                            },
                            Background::Color(handle_color),
                        );
                    }
                }
            };

            // Determine hover/drag state for each scrollbar
            let (is_v_hovered, is_h_hovered) = match status {
                Status::Hovered {
                    is_vertical_scrollbar_hovered,
                    is_horizontal_scrollbar_hovered,
                    ..
                } => (
                    is_vertical_scrollbar_hovered,
                    is_horizontal_scrollbar_hovered,
                ),
                _ => (false, false),
            };

            let (is_v_dragged, is_h_dragged) = match status {
                Status::Dragged {
                    is_vertical_scrollbar_dragged,
                    is_horizontal_scrollbar_dragged,
                    ..
                } => (
                    is_vertical_scrollbar_dragged,
                    is_horizontal_scrollbar_dragged,
                ),
                _ => (false, false),
            };

            renderer.with_layer(
                Rectangle {
                    width: (visible_bounds.width + 2.0).min(viewport.width),
                    height: (visible_bounds.height + 2.0).min(viewport.height),
                    ..visible_bounds
                },
                |renderer| {
                    if let Some(scrollbar) = scrollbars.y {
                        draw_scrollbar(
                            renderer,
                            scroll_style,
                            &scrollbar,
                            true, // is_vertical
                            is_v_hovered,
                            is_v_dragged,
                        );
                    }

                    if let Some(scrollbar) = scrollbars.x {
                        draw_scrollbar(
                            renderer,
                            scroll_style,
                            &scrollbar,
                            false, // is_vertical
                            is_h_hovered,
                            is_h_dragged,
                        );
                    }

                    if let (Some(x), Some(y)) = (scrollbars.x, scrollbars.y) {
                        let background = style.gap.or(style.container.background);

                        if let Some(background) = background {
                            renderer.fill_quad(
                                renderer::Quad {
                                    bounds: Rectangle {
                                        x: y.bounds.x,
                                        y: x.bounds.y,
                                        width: y.bounds.width,
                                        height: x.bounds.height,
                                    },
                                    ..renderer::Quad::default()
                                },
                                background,
                            );
                        }
                    }
                },
            );
        } else {
            // No scrolling needed, render content directly

            // Use cached content from layout(); see scrolling branch above
            // for rationale.
            let cached = self.cached_content.borrow();
            if let Some((_, content)) = cached.as_ref() {
                let Some(content_tree) = tree.children.first() else {
                    return;
                };

                if let Some(content_layout) = layout.children().next() {
                    content.as_widget().draw(
                        content_tree,
                        renderer,
                        theme,
                        defaults,
                        content_layout,
                        cursor,
                        &visible_bounds,
                    );
                }
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let cursor_over_scrollable = cursor.position_over(bounds);

        let (right_padding, bottom_padding) = embedded_padding(self.direction, state);
        let viewport_size = Size::new(
            (bounds.width - right_padding).max(0.0),
            (bounds.height - bottom_padding).max(0.0),
        );

        let content_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: if self.content_size.width > 0.0 {
                self.content_size.width
            } else {
                bounds.width
            },
            height: if self.content_size.height > 0.0 {
                self.content_size.height
            } else {
                bounds.height
            },
        };

        let scrollbars =
            Scrollbars::new(state, self.direction, bounds, viewport_size, content_bounds);
        let (mouse_over_y_scrollbar, mouse_over_x_scrollbar) = scrollbars.is_mouse_over(cursor);

        if state.scrollers_grabbed() {
            return mouse::Interaction::None;
        }

        let cursor = match cursor_over_scrollable {
            Some(cursor_position) if !(mouse_over_x_scrollbar || mouse_over_y_scrollbar) => {
                mouse::Cursor::Available(cursor_position)
            }
            _ => cursor.levitate(),
        };

        // Get mouse interaction from content

        // Use cached content from layout(); the tree was diffed against it.
        let cached = self.cached_content.borrow();
        if let Some((_, content)) = cached.as_ref() {
            let Some(content_tree) = tree.children.first() else {
                return mouse::Interaction::None;
            };

            if let Some(content_layout) = layout.children().next() {
                content.as_widget().mouse_interaction(
                    content_tree,
                    content_layout,
                    cursor,
                    &bounds,
                    renderer,
                )
            } else {
                mouse::Interaction::None
            }
        } else {
            mouse::Interaction::None
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        // Virtual scrollable doesn't support overlays from content.
        // However, we still render the auto-scroll indicator when active.
        let state = tree.state.downcast_ref::<State>();

        let Interaction::AutoScrolling { origin, .. } = state.interaction else {
            return None;
        };

        let bounds = layout.bounds();

        let (right_padding, bottom_padding) = embedded_padding(self.direction, state);
        let viewport = Size::new(
            (bounds.width - right_padding).max(0.0),
            (bounds.height - bottom_padding).max(0.0),
        );

        let content_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: if self.content_size.width > 0.0 {
                self.content_size.width
            } else {
                bounds.width
            },
            height: if self.content_size.height > 0.0 {
                self.content_size.height
            } else {
                bounds.height
            },
        };

        let scrollbars = Scrollbars::new(state, self.direction, bounds, viewport, content_bounds);

        Some(overlay::Element::new(Box::new(AutoScrollIcon {
            origin,
            vertical: scrollbars.y.is_some(),
            horizontal: scrollbars.x.is_some(),
            class: &self.class,
        })))
    }
}

struct AutoScrollIcon<'a, Class> {
    origin: Point,
    vertical: bool,
    horizontal: bool,
    class: &'a Class,
}

impl<Class> AutoScrollIcon<'_, Class> {
    const SIZE: f32 = 40.0;
    const DOT: f32 = Self::SIZE / 10.0;
    const PADDING: f32 = Self::SIZE / 10.0;
}

impl<Message, Theme, Renderer> core::Overlay<Message, Theme, Renderer>
    for AutoScrollIcon<'_, Theme::Class<'_>>
where
    Renderer: text::Renderer,
    Theme: Catalog,
{
    fn layout(&mut self, _renderer: &Renderer, _bounds: Size) -> layout::Node {
        layout::Node::new(Size::new(Self::SIZE, Self::SIZE))
            .move_to(self.origin - Vector::new(Self::SIZE, Self::SIZE) / 2.0)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
    ) {
        let bounds = layout.bounds();
        let style = theme
            .style(
                self.class,
                Status::Active {
                    hover_factor: 0.0,
                    is_horizontal_scrollbar_disabled: false,
                    is_vertical_scrollbar_disabled: false,
                },
            )
            .auto_scroll;

        renderer.with_layer(Rectangle::INFINITE, |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: style.border,
                    shadow: style.shadow,
                    snap: false,
                },
                style.background,
            );

            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        bounds.center() - Vector::new(Self::DOT, Self::DOT) / 2.0,
                        Size::new(Self::DOT, Self::DOT),
                    ),
                    border: border::rounded(bounds.width),
                    snap: false,
                    ..renderer::Quad::default()
                },
                style.icon,
            );

            let arrow = crate::core::Text {
                content: String::new(),
                bounds: bounds.size(),
                size: Pixels::from(12),
                line_height: text::LineHeight::Relative(1.0),
                font: Renderer::ICON_FONT,
                align_x: text::Alignment::Center,
                align_y: alignment::Vertical::Center,
                shaping: text::Shaping::Basic,
                wrapping: text::Wrapping::None,
                hint_factor: None,
            };

            if self.vertical {
                renderer.fill_text(
                    crate::core::Text {
                        content: Renderer::SCROLL_UP_ICON.to_string(),
                        align_y: alignment::Vertical::Top,
                        ..arrow
                    },
                    Point::new(bounds.center_x(), bounds.y + Self::PADDING),
                    style.icon,
                    bounds,
                );

                renderer.fill_text(
                    crate::core::Text {
                        content: Renderer::SCROLL_DOWN_ICON.to_string(),
                        align_y: alignment::Vertical::Bottom,
                        ..arrow
                    },
                    Point::new(
                        bounds.center_x(),
                        bounds.y + bounds.height - Self::PADDING - 0.5,
                    ),
                    style.icon,
                    bounds,
                );
            }

            if self.horizontal {
                let is_rtl = crate::core::layout_direction().is_rtl();

                // In RTL, swap left/right arrow positions and icons
                let (left_icon, right_icon) = if is_rtl {
                    (Renderer::SCROLL_RIGHT_ICON, Renderer::SCROLL_LEFT_ICON)
                } else {
                    (Renderer::SCROLL_LEFT_ICON, Renderer::SCROLL_RIGHT_ICON)
                };

                renderer.fill_text(
                    crate::core::Text {
                        content: left_icon.to_string(),
                        align_x: text::Alignment::Left,
                        ..arrow
                    },
                    Point::new(bounds.x + Self::PADDING + 1.0, bounds.center_y() + 1.0),
                    style.icon,
                    bounds,
                );

                renderer.fill_text(
                    crate::core::Text {
                        content: right_icon.to_string(),
                        align_x: text::Alignment::Right,
                        ..arrow
                    },
                    Point::new(
                        bounds.x + bounds.width - Self::PADDING - 1.0,
                        bounds.center_y() + 1.0,
                    ),
                    style.icon,
                    bounds,
                );
            }
        });
    }

    fn index(&self) -> f32 {
        f32::MAX
    }
}

impl<'a, Message, Theme, Renderer> From<VirtualScrollable<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a + Catalog,
    Renderer: 'a + text::Renderer,
{
    fn from(scrollable: VirtualScrollable<'a, Message, Theme, Renderer>) -> Self {
        Element::new(scrollable)
    }
}

fn notify_scroll<Message>(
    state: &mut State,
    on_scroll: &Option<Box<dyn Fn(Viewport) -> Message + '_>>,
    bounds: Rectangle,
    content_bounds: Rectangle,
    shell: &mut Shell<'_, Message>,
) -> bool {
    if notify_viewport(state, on_scroll, bounds, content_bounds, shell) {
        state.last_scrolled = Some(Instant::now());
        true
    } else {
        false
    }
}

fn notify_viewport<Message>(
    state: &mut State,
    on_scroll: &Option<Box<dyn Fn(Viewport) -> Message + '_>>,
    bounds: Rectangle,
    content_bounds: Rectangle,
    shell: &mut Shell<'_, Message>,
) -> bool {
    if content_bounds.width <= bounds.width && content_bounds.height <= bounds.height {
        return false;
    }

    // Compute absolute offset from the internal Offset values
    let offset = AbsoluteOffset {
        x: state.offset_x.absolute(bounds.width, content_bounds.width),
        y: state
            .offset_y
            .absolute(bounds.height, content_bounds.height),
    };

    let viewport = Viewport::from_absolute(offset, bounds, content_bounds);

    if let Some(last_notified) = state.last_notified {
        let last_relative_offset = last_notified.relative_offset();
        let current_relative_offset = viewport.relative_offset();

        let last_absolute_offset = last_notified.absolute_offset();
        let current_absolute_offset = viewport.absolute_offset();

        let unchanged =
            |a: f32, b: f32| (a - b).abs() <= f32::EPSILON || (a.is_nan() && b.is_nan());

        if last_notified.bounds() == bounds
            && last_notified.content_bounds() == content_bounds
            && unchanged(last_relative_offset.x, current_relative_offset.x)
            && unchanged(last_relative_offset.y, current_relative_offset.y)
            && unchanged(last_absolute_offset.x, current_absolute_offset.x)
            && unchanged(last_absolute_offset.y, current_absolute_offset.y)
        {
            return false;
        }
    }

    state.last_notified = Some(viewport);

    if let Some(on_scroll) = on_scroll {
        shell.publish(on_scroll(viewport));
    }

    true
}

#[derive(Debug)]
struct Scrollbars {
    y: Option<internals::Scrollbar>,
    x: Option<internals::Scrollbar>,
}

impl Scrollbars {
    fn new(
        state: &State,
        direction: Direction,
        bounds: Rectangle,
        viewport: Size,
        content_bounds: Rectangle,
    ) -> Self {
        let viewport_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: viewport.width,
            height: viewport.height,
        };

        let translation = state.translation(direction, viewport_bounds, content_bounds);

        let reserved_width = (bounds.width - viewport.width).max(0.0);
        let reserved_height = (bounds.height - viewport.height).max(0.0);

        let show_scrollbar_x = direction
            .horizontal()
            .filter(|_scrollbar| content_bounds.width > viewport.width);

        let show_scrollbar_y = direction
            .vertical()
            .filter(|_scrollbar| content_bounds.height > viewport.height);

        let y_scrollbar = if let Some(vertical) = show_scrollbar_y {
            let Scrollbar {
                width,
                margin,
                scroller_width,
                ..
            } = *vertical;

            let x_scrollbar_height = if reserved_height > 0.0 {
                0.0
            } else {
                show_scrollbar_x.map_or(0.0, |h| h.width.max(h.scroller_width) + 2.0 * h.margin)
            };

            let total_scrollbar_width = width.max(scroller_width) + 2.0 * margin;

            let total_scrollbar_bounds = Rectangle {
                x: bounds.x + bounds.width - total_scrollbar_width,
                y: bounds.y,
                width: total_scrollbar_width,
                height: (viewport.height - x_scrollbar_height).max(0.0),
            };

            let scrollbar_bounds = Rectangle {
                x: bounds.x + bounds.width - total_scrollbar_width / 2.0 - width / 2.0,
                y: bounds.y,
                width,
                height: (viewport.height - x_scrollbar_height).max(0.0),
            };

            let ratio = viewport.height / content_bounds.height;

            let scroller = if ratio >= 1.0 {
                None
            } else {
                let scroller_height = (scrollbar_bounds.height * ratio).max(2.0);
                let scroller_offset =
                    translation.y * ratio * scrollbar_bounds.height / viewport.height;

                let scroller_bounds = Rectangle {
                    x: bounds.x + bounds.width - total_scrollbar_width / 2.0 - scroller_width / 2.0,
                    y: (scrollbar_bounds.y + scroller_offset).max(0.0),
                    width: scroller_width,
                    height: scroller_height,
                };

                Some(internals::Scroller {
                    bounds: scroller_bounds,
                })
            };

            Some(internals::Scrollbar {
                total_bounds: total_scrollbar_bounds,
                bounds: scrollbar_bounds,
                scroller,
                alignment: vertical.alignment,
                disabled: content_bounds.height <= bounds.height,
            })
        } else {
            None
        };

        let x_scrollbar = if let Some(horizontal) = show_scrollbar_x {
            let Scrollbar {
                width,
                margin,
                scroller_width,
                ..
            } = *horizontal;

            let scrollbar_y_width = if reserved_width > 0.0 {
                0.0
            } else {
                y_scrollbar.map_or(0.0, |scrollbar| scrollbar.total_bounds.width)
            };

            let total_scrollbar_height = width.max(scroller_width) + 2.0 * margin;

            let total_scrollbar_bounds = Rectangle {
                x: bounds.x,
                y: bounds.y + bounds.height - total_scrollbar_height,
                width: (viewport.width - scrollbar_y_width).max(0.0),
                height: total_scrollbar_height,
            };

            let scrollbar_bounds = Rectangle {
                x: bounds.x,
                y: bounds.y + bounds.height - total_scrollbar_height / 2.0 - width / 2.0,
                width: (viewport.width - scrollbar_y_width).max(0.0),
                height: width,
            };

            let ratio = viewport.width / content_bounds.width;

            let scroller = if ratio >= 1.0 {
                None
            } else {
                let scroller_length = (scrollbar_bounds.width * ratio).max(2.0);
                let scroller_offset =
                    translation.x * ratio * scrollbar_bounds.width / viewport.width;

                let scroller_bounds = Rectangle {
                    x: (scrollbar_bounds.x + scroller_offset).max(0.0),
                    y: bounds.y + bounds.height
                        - total_scrollbar_height / 2.0
                        - scroller_width / 2.0,
                    width: scroller_length,
                    height: scroller_width,
                };

                Some(internals::Scroller {
                    bounds: scroller_bounds,
                })
            };

            Some(internals::Scrollbar {
                total_bounds: total_scrollbar_bounds,
                bounds: scrollbar_bounds,
                scroller,
                alignment: horizontal.alignment,
                disabled: content_bounds.width <= bounds.width,
            })
        } else {
            None
        };

        Self {
            y: y_scrollbar,
            x: x_scrollbar,
        }
    }

    fn is_mouse_over(&self, cursor: mouse::Cursor) -> (bool, bool) {
        if let Some(cursor_position) = cursor.position() {
            (
                self.y
                    .as_ref()
                    .map(|scrollbar| scrollbar.is_mouse_over(cursor_position))
                    .unwrap_or(false),
                self.x
                    .as_ref()
                    .map(|scrollbar| scrollbar.is_mouse_over(cursor_position))
                    .unwrap_or(false),
            )
        } else {
            (false, false)
        }
    }

    fn is_y_disabled(&self) -> bool {
        self.y.map(|y| y.disabled).unwrap_or(false)
    }

    fn is_x_disabled(&self) -> bool {
        self.x.map(|x| x.disabled).unwrap_or(false)
    }

    fn grab_y_scroller(&self, cursor_position: Point) -> Option<f32> {
        let scrollbar = self.y?;
        let scroller = scrollbar.scroller?;

        if scrollbar.total_bounds.contains(cursor_position) {
            Some(if scroller.bounds.contains(cursor_position) {
                (cursor_position.y - scroller.bounds.y) / scroller.bounds.height
            } else {
                0.5
            })
        } else {
            None
        }
    }

    fn grab_x_scroller(&self, cursor_position: Point) -> Option<f32> {
        let scrollbar = self.x?;
        let scroller = scrollbar.scroller?;

        if scrollbar.total_bounds.contains(cursor_position) {
            Some(if scroller.bounds.contains(cursor_position) {
                (cursor_position.x - scroller.bounds.x) / scroller.bounds.width
            } else {
                0.5
            })
        } else {
            None
        }
    }

    fn active(&self) -> bool {
        self.y.is_some() || self.x.is_some()
    }
}

mod internals {
    use super::Anchor;
    use crate::core::{Point, Rectangle};

    #[derive(Debug, Copy, Clone)]
    pub struct Scrollbar {
        pub total_bounds: Rectangle,
        pub bounds: Rectangle,
        pub scroller: Option<Scroller>,
        pub alignment: Anchor,
        pub disabled: bool,
    }

    impl Scrollbar {
        pub fn is_mouse_over(&self, cursor_position: Point) -> bool {
            self.total_bounds.contains(cursor_position)
        }

        pub fn scroll_percentage_y(&self, grabbed_at: f32, cursor_position: Point) -> f32 {
            if let Some(scroller) = self.scroller {
                let percentage =
                    (cursor_position.y - self.bounds.y - scroller.bounds.height * grabbed_at)
                        / (self.bounds.height - scroller.bounds.height);

                match self.alignment {
                    Anchor::Start => percentage,
                    Anchor::End => 1.0 - percentage,
                }
            } else {
                0.0
            }
        }

        pub fn scroll_percentage_x(&self, grabbed_at: f32, cursor_position: Point) -> f32 {
            if let Some(scroller) = self.scroller {
                let percentage =
                    (cursor_position.x - self.bounds.x - scroller.bounds.width * grabbed_at)
                        / (self.bounds.width - scroller.bounds.width);

                match self.alignment {
                    Anchor::Start => percentage,
                    Anchor::End => 1.0 - percentage,
                }
            } else {
                0.0
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Scroller {
        pub bounds: Rectangle,
    }
}

/// Compares two Status values ignoring the hover_factor field.
/// Returns true if any field other than hover_factor differs.
fn status_fields_changed(a: Status, b: Status) -> bool {
    match (a, b) {
        (
            Status::Active {
                is_horizontal_scrollbar_disabled: h1,
                is_vertical_scrollbar_disabled: v1,
                ..
            },
            Status::Active {
                is_horizontal_scrollbar_disabled: h2,
                is_vertical_scrollbar_disabled: v2,
                ..
            },
        ) => h1 != h2 || v1 != v2,
        (
            Status::Hovered {
                is_horizontal_scrollbar_hovered: hh1,
                is_vertical_scrollbar_hovered: vh1,
                is_horizontal_scrollbar_disabled: hd1,
                is_vertical_scrollbar_disabled: vd1,
                ..
            },
            Status::Hovered {
                is_horizontal_scrollbar_hovered: hh2,
                is_vertical_scrollbar_hovered: vh2,
                is_horizontal_scrollbar_disabled: hd2,
                is_vertical_scrollbar_disabled: vd2,
                ..
            },
        ) => hh1 != hh2 || vh1 != vh2 || hd1 != hd2 || vd1 != vd2,
        (
            Status::Dragged {
                is_horizontal_scrollbar_dragged: hd1,
                is_vertical_scrollbar_dragged: vd1,
                is_horizontal_scrollbar_disabled: hdi1,
                is_vertical_scrollbar_disabled: vdi1,
                ..
            },
            Status::Dragged {
                is_horizontal_scrollbar_dragged: hd2,
                is_vertical_scrollbar_dragged: vd2,
                is_horizontal_scrollbar_disabled: hdi2,
                is_vertical_scrollbar_disabled: vdi2,
                ..
            },
        ) => hd1 != hd2 || vd1 != vd2 || hdi1 != hdi2 || vdi1 != vdi2,
        // Different variants - discriminant check handles this, but for safety:
        _ => true,
    }
}
