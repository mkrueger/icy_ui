// From libcosmic, license MPL-2.0
// Ported to icy

//! A context menu is a menu in a graphical user interface that appears upon
//! user interaction, such as a right-click mouse operation.

use super::app_menu::convert_children;
use super::menu_bar::{MenuBarState, menu_roots_diff};
use super::menu_inner::{CloseCondition, Direction, ItemHeight, ItemWidth, Menu, PathHighlight};
use super::menu_tree::MenuTree;
use super::style::StyleSheet;

use crate::core::menu::{MenuId, MenuKind, MenuNode};
use crate::core::renderer;
use crate::core::{
    Clipboard, Element, Event, Layout, LayoutDirection, Length, Point, Rectangle, Shell, Size,
    Vector, Widget, keyboard, mouse, overlay, touch,
    widget::{Tree, tree},
};

use std::collections::HashSet;

/// Finds the message for an activated menu item by its [`MenuId`].
///
/// This function searches through the provided menu nodes to find
/// the item with the matching ID and returns a clone of its `on_activate`
/// message.
fn find_activated_message<Message>(id: &MenuId, nodes: &[MenuNode<Message>]) -> Option<Message>
where
    Message: Clone,
{
    for node in nodes {
        if &node.id == id {
            match &node.kind {
                MenuKind::Item { on_activate, .. } => {
                    return Some(on_activate.clone());
                }
                MenuKind::CheckItem { on_activate, .. } => {
                    return Some(on_activate.clone());
                }
                _ => {}
            }
        }

        // Recursively search submenus
        if let MenuKind::Submenu { children, .. } = &node.kind {
            if let Some(msg) = find_activated_message(id, children) {
                return Some(msg);
            }
        }
    }

    None
}

/// A context menu is a menu in a graphical user interface that appears upon
/// user interaction, such as a right-click mouse operation.
pub fn context_menu_from<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    menu: Option<Vec<MenuTree<'a, Message, Theme, Renderer>>>,
) -> ContextMenu<'a, Message, Theme, Renderer>
where
    Message: Clone + 'static,
    Renderer: renderer::Renderer + 'a,
    Theme: StyleSheet + crate::text::Catalog + 'a,
{
    let mut this = ContextMenu {
        content: content.into(),
        context_menu: menu.map(|menus| {
            vec![MenuTree::with_children(
                Element::from(crate::Row::<'static, Message, Theme, Renderer>::new()),
                menus,
            )]
        }),
        close_on_escape: true,
        on_right_click: None,
        #[cfg(target_os = "macos")]
        native_items: None, // context_menu_from doesn't have native items
        menu_nodes: None, // context_menu_from doesn't have menu nodes
        layout_direction: None,
    };

    if let Some(ref mut context_menu) = this.context_menu {
        context_menu.iter_mut().for_each(MenuTree::set_index);
    }

    this
}

/// Creates a context menu from [`MenuNode`] items.
///
/// This is a convenience function that converts the simpler `MenuNode` API
/// into a context menu widget, making it easier to create consistent menus
/// between the application menu and context menus.
///
/// On macOS, this will automatically show a native context menu.
/// On other platforms, an overlay menu will be displayed.
///
/// # Example
///
/// ```ignore
/// use icy_ui_core::menu;
/// use icy_ui::widget::menu::context_menu;
///
/// let nodes = vec![
///     menu::item!("Cut", Message::Cut),
///     menu::item!("Copy", Message::Copy),
///     menu::separator!(),
///     menu::item!("Paste", Message::Paste),
/// ];
///
/// context_menu(my_content, &nodes)
/// ```
pub fn context_menu<'a, Message>(
    content: impl Into<Element<'a, Message, crate::Theme, crate::Renderer>>,
    nodes: &[MenuNode<Message>],
) -> ContextMenu<'a, Message, crate::Theme, crate::Renderer>
where
    Message: Clone + 'static,
{
    #[cfg(target_os = "macos")]
    use crate::core::menu::ContextMenuItem;

    #[cfg(target_os = "macos")]
    let native_items = ContextMenuItem::from_menu_nodes(nodes);
    let menu_nodes = nodes.to_vec();
    let items = convert_children(&menu_nodes);

    let mut this = ContextMenu {
        content: content.into(),
        context_menu: if items.is_empty() {
            None
        } else {
            Some(vec![MenuTree::with_children(
                Element::from(crate::Row::<'static, Message, crate::Theme, crate::Renderer>::new()),
                items,
            )])
        },
        close_on_escape: true,
        on_right_click: None,
        #[cfg(target_os = "macos")]
        native_items: if native_items.is_empty() {
            None
        } else {
            Some(native_items)
        },
        menu_nodes: if menu_nodes.is_empty() {
            None
        } else {
            Some(menu_nodes)
        },
        layout_direction: None,
    };

    if let Some(ref mut context_menu) = this.context_menu {
        context_menu.iter_mut().for_each(MenuTree::set_index);
    }

    this
}

/// A context menu is a menu in a graphical user interface that appears upon
/// user interaction, such as a right-click mouse operation.
#[must_use]
pub struct ContextMenu<'a, Message, Theme = crate::Theme, Renderer = crate::Renderer>
where
    Renderer: renderer::Renderer,
{
    content: Element<'a, Message, Theme, Renderer>,
    context_menu: Option<Vec<MenuTree<'a, Message, Theme, Renderer>>>,
    /// Whether to close on escape key press
    pub close_on_escape: bool,
    /// Optional callback for right-click events.
    /// When set, this callback is invoked instead of showing the overlay menu.
    /// This is useful for triggering native context menus on macOS.
    on_right_click: Option<Box<dyn Fn(Point) -> Message + 'a>>,
    /// Native menu items for platforms that support native context menus.
    /// This is used on macOS to show a native NSMenu.
    #[cfg(target_os = "macos")]
    native_items: Option<Vec<crate::core::menu::ContextMenuItem>>,
    /// The original menu nodes, used to look up messages when a native menu item is selected.
    menu_nodes: Option<Vec<MenuNode<Message>>>,
    /// Override for layout direction. If `None`, uses the global style direction.
    layout_direction: Option<LayoutDirection>,
}

impl<'a, Message, Theme, Renderer> ContextMenu<'a, Message, Theme, Renderer>
where
    Message: Clone + 'static,
    Renderer: renderer::Renderer + 'a,
    Theme: StyleSheet + 'a,
{
    /// Set whether to close on escape key press
    pub fn close_on_escape(mut self, close_on_escape: bool) -> Self {
        self.close_on_escape = close_on_escape;
        self
    }

    /// Set a callback to be invoked on right-click instead of showing the overlay menu.
    ///
    /// When this is set, the widget will not show its overlay menu on right-click.
    /// Instead, it will publish a message returned by the callback function.
    /// This is useful for triggering native context menus on macOS.
    ///
    /// # Example
    ///
    /// ```ignore
    /// context_menu(content, &nodes)
    ///     .on_right_click(|pos| Message::ShowNativeContextMenu(pos))
    /// ```
    pub fn on_right_click(mut self, callback: impl Fn(Point) -> Message + 'a) -> Self {
        self.on_right_click = Some(Box::new(callback));
        self
    }

    /// Sets the layout direction for this context menu.
    ///
    /// When set to RTL, submenus will open to the left instead of the right.
    /// If not set, uses the global style direction.
    pub fn layout_direction(mut self, direction: LayoutDirection) -> Self {
        self.layout_direction = Some(direction);
        self
    }
}

struct LocalState {
    context_cursor: Point,
    fingers_pressed: HashSet<touch::Finger>,
    menu_bar_state: MenuBarState,
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for ContextMenu<'a, Message, Theme, Renderer>
where
    Message: Clone + 'static,
    Renderer: renderer::Renderer + 'a,
    Theme: StyleSheet + 'a,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<LocalState>()
    }

    fn state(&self) -> tree::State {
        #[allow(clippy::default_trait_access)]
        tree::State::new(LocalState {
            context_cursor: Point::default(),
            fingers_pressed: Default::default(),
            menu_bar_state: Default::default(),
        })
    }

    fn children(&self) -> Vec<Tree> {
        let mut children = Vec::with_capacity(if self.context_menu.is_some() { 2 } else { 1 });

        children.push(Tree::new(self.content.as_widget()));

        // Assign the context menu's elements as this widget's children.
        if let Some(ref context_menu) = self.context_menu {
            let mut tree = Tree::empty();
            tree.children = context_menu
                .iter()
                .map(|root| {
                    let mut tree = Tree::empty();
                    let flat = root
                        .flatten()
                        .iter()
                        .map(|mt| Tree::new(mt.item.as_widget()))
                        .collect();
                    tree.children = flat;
                    tree
                })
                .collect();

            children.push(tree);
        }

        children
    }

    fn diff(&self, tree: &mut Tree) {
        tree.children[0].diff(self.content.as_widget());
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &crate::core::layout::Limits,
    ) -> crate::core::layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &crate::core::renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // Set horizontal direction based on widget override or global
        let local_state = tree.state.downcast_ref::<LocalState>();
        let direction = self
            .layout_direction
            .unwrap_or_else(crate::core::layout_direction);
        local_state.menu_bar_state.inner.with_data_mut(|state| {
            state.horizontal_direction = if direction.is_rtl() {
                Direction::Negative
            } else {
                Direction::Positive
            };
        });

        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn crate::core::widget::Operation<()>,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Handle native context menu item selection
        // When a native menu item is selected, look up the message and publish it
        if let Event::ContextMenuItemSelected(menu_id) = event {
            if let Some(ref nodes) = self.menu_nodes {
                if let Some(msg) = find_activated_message(menu_id, nodes) {
                    shell.publish(msg);
                    shell.capture_event();
                    return;
                }
            }
        }

        let state = tree.state.downcast_mut::<LocalState>();
        let bounds = layout.bounds();

        let open = state.menu_bar_state.inner.with_data(|state| state.open);
        let mut was_open = false;

        // Handle escape key when menu is open
        let close_for_escape = self.close_on_escape
            && matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                })
            );

        // Handle clicks when menu is open
        let close_for_click = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed {
                button: mouse::Button::Right | mouse::Button::Left,
                ..
            }) | Event::Touch(touch::Event::FingerPressed { .. })
        );

        if open && (close_for_escape || close_for_click) {
            state.menu_bar_state.inner.with_data_mut(|state| {
                was_open = true;
                state.menu_states.clear();
                state.active_root.clear();
                state.open = false;
            });
            shell.capture_event();
            shell.request_redraw();

            // When closing because of a right-click press, immediately reopen
            // the context menu at the new position.  This is needed because
            // the overlay event loop may have already captured the matching
            // ButtonReleased(Right) via its `click_outside` close logic,
            // preventing the widget tree from ever seeing the release event
            // that would normally trigger the reopen.
            if self.context_menu.is_some() && cursor.is_over(bounds) && right_button_pressed(event)
            {
                state.context_cursor = cursor.position().unwrap_or_default();
                state.menu_bar_state.inner.with_data_mut(|state| {
                    state.open = true;
                    state.view_cursor = cursor;
                    state.active_root.clear();
                    state.active_root.push(0);
                });
            }
        }

        if !was_open && cursor.is_over(bounds) {
            let fingers_pressed = state.fingers_pressed.len();

            match event {
                Event::Touch(touch::Event::FingerPressed { id, .. }) => {
                    let _ = state.fingers_pressed.insert(*id);
                }

                Event::Touch(touch::Event::FingerLifted { id, .. }) => {
                    let _ = state.fingers_pressed.remove(id);
                }

                _ => (),
            }

            // Present a context menu on a right click event.
            if !was_open
                && self.context_menu.is_some()
                && (right_button_released(event) || (touch_lifted(event) && fingers_pressed == 2))
            {
                state.context_cursor = cursor.position().unwrap_or_default();

                // If on_right_click callback is set, publish message instead of showing overlay
                if let Some(ref on_right_click) = self.on_right_click {
                    let message = on_right_click(state.context_cursor);
                    shell.publish(message);
                    shell.capture_event();
                    return;
                }

                // On macOS, use native context menu if we have native items
                #[cfg(target_os = "macos")]
                if let Some(ref items) = self.native_items {
                    shell.request_context_menu(state.context_cursor, items.clone());
                    shell.capture_event();
                    return;
                }

                // Fallback to overlay menu
                state.menu_bar_state.inner.with_data_mut(|state| {
                    state.open = true;
                    state.view_cursor = cursor;
                    // Set active_root to trigger immediate menu initialization
                    state.active_root.clear();
                    state.active_root.push(0);
                });

                shell.capture_event();
                shell.request_redraw();
                // Don't return early — let the content widget also see the
                // ButtonReleased event so it can clean up any in-progress
                // state (e.g. drag tracking started on the matching
                // ButtonPressed).
            } else if !was_open
                && (right_button_released(event)
                    || touch_lifted(event)
                    || left_button_released(event))
            {
                state.menu_bar_state.inner.with_data_mut(|state| {
                    state.menu_states.clear();
                    state.active_root.clear();
                    state.open = false;
                });
                shell.request_redraw();
            }
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        _layout: Layout<'_>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let state = tree.state.downcast_ref::<LocalState>();

        let context_menu = self.context_menu.as_mut()?;

        if !state.menu_bar_state.inner.with_data(|state| state.open) {
            return None;
        }

        // Sync trees
        state.menu_bar_state.inner.with_data_mut(|inner| {
            menu_roots_diff(context_menu, &mut inner.tree);
        });

        // Create a point-sized bounds at the context cursor position.
        // The menu will open from this point.
        let context_point = Point::new(
            state.context_cursor.x - translation.x,
            state.context_cursor.y - translation.y,
        );
        let bounds = Rectangle::new(context_point, Size::ZERO);

        Some(
            Menu {
                tree: state.menu_bar_state.clone(),
                menu_roots: context_menu,
                bounds_expand: 16,
                menu_overlays_parent: true,
                close_condition: CloseCondition {
                    leave: false,
                    click_outside: true,
                    click_inside: true,
                },
                item_width: ItemWidth::Dynamic { min: 180, max: 600 },
                item_height: ItemHeight::Dynamic(40),
                bar_bounds: bounds,
                main_offset: 0,
                cross_offset: 0,
                root_bounds_list: vec![bounds],
                path_highlight: Some(PathHighlight::MenuActive),
                style: <Theme as StyleSheet>::Style::default(),
                position: Point::new(translation.x, translation.y),
                is_overlay: true,
            }
            .overlay(),
        )
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<ContextMenu<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'static,
    Renderer: renderer::Renderer + 'a,
    Theme: StyleSheet + 'a,
{
    fn from(widget: ContextMenu<'a, Message, Theme, Renderer>) -> Self {
        Self::new(widget)
    }
}

fn right_button_released(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonReleased {
            button: mouse::Button::Right,
            ..
        })
    )
}

fn right_button_pressed(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonPressed {
            button: mouse::Button::Right,
            ..
        })
    )
}

fn left_button_released(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonReleased {
            button: mouse::Button::Left,
            ..
        })
    )
}

fn touch_lifted(event: &Event) -> bool {
    matches!(event, Event::Touch(touch::Event::FingerLifted { .. }))
}
