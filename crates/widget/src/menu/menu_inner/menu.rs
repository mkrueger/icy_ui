// From iced_aw, license MIT
// Ported from libcosmic

//! Menu struct and implementation

use super::bounds::{Aod, MenuBounds};
use super::helpers::{
    init_root_menu, pad_rectangle, process_menu_events, process_overlay_events,
    process_scroll_events,
};
use super::state::MenuState;
use super::types::{CloseCondition, Direction, ItemHeight, ItemWidth, PathHighlight};
use crate::core::layout::{Limits, Node};
use crate::core::mouse::Cursor;
use crate::core::widget::Tree;
use crate::core::{
    Border, Clipboard, Layout, Length, Point, Rectangle, Shadow, Shell, Size, Vector, event,
    keyboard, mouse, overlay, renderer, touch,
};
use crate::menu::menu_bar::MenuBarState;
use crate::menu::menu_tree::MenuTree;
use crate::menu::mnemonic::{mnemonics_enabled, set_show_underlines};
use crate::menu::style::StyleSheet;

pub(in crate::menu) struct Menu<'a, 'b, Message, Theme = crate::Theme, Renderer = crate::Renderer>
where
    Message: Clone,
    Renderer: renderer::Renderer,
    Theme: StyleSheet,
{
    pub(in crate::menu) tree: MenuBarState,
    pub(in crate::menu) menu_roots: &'b mut [MenuTree<'a, Message, Theme, Renderer>],
    pub(in crate::menu) bounds_expand: u16,
    pub(in crate::menu) menu_overlays_parent: bool,
    pub(in crate::menu) close_condition: CloseCondition,
    pub(in crate::menu) item_width: ItemWidth,
    pub(in crate::menu) item_height: ItemHeight,
    pub(in crate::menu) bar_bounds: Rectangle,
    pub(in crate::menu) main_offset: i32,
    pub(in crate::menu) cross_offset: i32,
    pub(in crate::menu) root_bounds_list: Vec<Rectangle>,
    pub(in crate::menu) path_highlight: Option<PathHighlight>,
    pub(in crate::menu) style: Theme::Style,
    pub(in crate::menu) position: Point,
    pub(in crate::menu) is_overlay: bool,
}

impl<'a, 'b, Message, Theme, Renderer> Menu<'a, 'b, Message, Theme, Renderer>
where
    Message: Clone,
    Renderer: renderer::Renderer,
    Theme: StyleSheet,
{
    pub(crate) fn overlay(self) -> overlay::Element<'b, Message, Theme, Renderer>
    where
        'a: 'b,
    {
        overlay::Element::new(Box::new(self))
    }

    pub(crate) fn layout(&mut self, renderer: &Renderer, limits: Limits) -> Node {
        let position = self.position;
        let mut intrinsic_size = Size::ZERO;
        let menu_roots = &mut self.menu_roots;
        let is_overlay = self.is_overlay;
        let item_height = self.item_height;

        self.tree.inner.with_data_mut(|data| {
            if data.active_root.is_empty() || data.menu_states.is_empty() {
                return Node::new(limits.min());
            }

            let overlay_offset = Point::ORIGIN - position;
            let tree_children: &mut Vec<Tree> = &mut data.tree.children;

            let children: Vec<Node> = data
                .menu_states
                .iter()
                .enumerate()
                .filter_map(|(i, ms)| {
                    if !is_overlay && i > 0 {
                        return None;
                    }

                    let active_root = &data.active_root;
                    if active_root.is_empty() {
                        return None;
                    }

                    let first_root = *active_root.first()?;
                    let Some(root_entry) = menu_roots.get_mut(first_root) else {
                        return None;
                    };
                    let Some(tree_entry) = tree_children.get_mut(first_root) else {
                        return None;
                    };

                    // Each `MenuState` corresponds to a depth in `active_root`.
                    // Depth 0 (root menu) should use `root_entry.children`.
                    // Depth 1 should use `root_entry.children[active_root[1]].children`, etc.
                    //
                    // IMPORTANT: `tree_entry.children` is a *flat* widget tree (built from
                    // `MenuTree::flatten()`), and menu items reference it via `MenuTree::index`.
                    // Therefore, we must always pass the full flat tree slice for the root.
                    let menu_items = active_root.iter().skip(1).take(i).fold(
                        &mut root_entry.children,
                        |mt, &next_active_root| {
                            if next_active_root < mt.len() {
                                &mut mt[next_active_root].children
                            } else {
                                mt
                            }
                        },
                    );

                    let flat_tree = &mut tree_entry.children;

                    let slice = ms.slice(limits.max(), overlay_offset, item_height);
                    let children_node =
                        ms.layout(overlay_offset, slice, renderer, menu_items, flat_tree);
                    let node_size = children_node.size();
                    intrinsic_size.height += node_size.height;
                    intrinsic_size.width = intrinsic_size.width.max(node_size.width);

                    Some(children_node)
                })
                .collect();

            Node::with_children(
                limits.resolve(Length::Shrink, Length::Shrink, intrinsic_size),
                children,
            )
            .translate(Point::ORIGIN - position)
        })
    }

    pub(crate) fn operate_inner(
        &mut self,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn crate::core::widget::Operation,
    ) {
        let viewport = layout.bounds();
        let viewport_size = viewport.size();
        let overlay_offset = Point::ORIGIN - viewport.position();

        operation.container(None, viewport);

        #[cfg(feature = "accessibility")]
        {
            let is_overlay = self.is_overlay;
            let menu_roots: &mut [MenuTree<'a, Message, Theme, Renderer>] = &mut self.menu_roots;
            let item_height = self.item_height;

            self.tree.inner.with_data(|state| {
                if !state.open || state.active_root.is_empty() {
                    return;
                }

                let Some(first_root) = state.active_root.first().copied() else {
                    return;
                };
                let Some(root_entry) = menu_roots.get(first_root) else {
                    return;
                };

                for (panel_index, (ms, panel_layout)) in
                    state.menu_states.iter().zip(layout.children()).enumerate()
                {
                    if !is_overlay && panel_index > 0 {
                        continue;
                    }

                    let menu_items: &[_] = state.active_root.iter().skip(1).take(panel_index).fold(
                        &root_entry.children[..],
                        |mt, &next_active_root| {
                            mt.get(next_active_root)
                                .map(|m| &m.children[..])
                                .unwrap_or(mt)
                        },
                    );

                    let slice = ms.slice(viewport_size, overlay_offset, item_height);
                    let start_index = slice.start_index;
                    let end_index = slice.end_index;

                    let panel_id = crate::core::widget::Id::from(format!(
                        "icy_ui.menu/{}/panel/{}",
                        first_root, panel_index
                    ));

                    // Report ALL menu items as children (not just visible slice) for accessibility
                    // This ensures arrow-key navigation can always find the focused item.
                    let child_ids = (0..menu_items.len())
                        .map(|item_index| {
                            let item_id = crate::core::widget::Id::from(format!(
                                "icy_ui.menu/{}/panel/{}/item/{}",
                                first_root, panel_index, item_index
                            ));
                            crate::core::accessibility::node_id_from_widget_id(&item_id)
                        })
                        .collect::<Vec<_>>();

                    let panel_bounds = panel_layout.bounds();
                    let panel_info = crate::core::accessibility::WidgetInfo::menu()
                        .with_bounds(panel_bounds)
                        .with_expanded(Some(true))
                        .with_children(child_ids);
                    operation.accessibility(Some(&panel_id), panel_bounds, panel_info);

                    // Emit ALL menu items for accessibility (not just visible ones).
                    // For visible items, use actual layout bounds. For off-screen items, use stored positions/sizes.
                    let base_y = panel_bounds.y;
                    let child_positions = &ms.menu_bounds.child_positions;
                    let child_sizes = &ms.menu_bounds.child_sizes;

                    for (item_index, mt) in menu_items.iter().enumerate() {
                        let item_id = crate::core::widget::Id::from(format!(
                            "icy_ui.menu/{}/panel/{}/item/{}",
                            first_root, panel_index, item_index
                        ));

                        // Calculate bounds - use actual layout for visible items, stored data for others
                        let bounds = if item_index >= start_index && item_index <= end_index {
                            // Visible item - get from layout
                            let visible_idx = item_index - start_index;
                            panel_layout
                                .children()
                                .nth(visible_idx)
                                .map(|l| l.bounds())
                                .unwrap_or_else(|| {
                                    // Fallback to stored position/size
                                    let y_offset =
                                        child_positions.get(item_index).copied().unwrap_or(0.0);
                                    let height = child_sizes
                                        .get(item_index)
                                        .map(|s| s.height)
                                        .unwrap_or(36.0);
                                    crate::core::Rectangle {
                                        x: panel_bounds.x,
                                        y: base_y + y_offset,
                                        width: panel_bounds.width,
                                        height,
                                    }
                                })
                        } else {
                            // Off-screen item - use stored position/size
                            let y_offset = child_positions.get(item_index).copied().unwrap_or(0.0);
                            let height = child_sizes
                                .get(item_index)
                                .map(|s| s.height)
                                .unwrap_or(36.0);
                            crate::core::Rectangle {
                                x: panel_bounds.x,
                                y: base_y + y_offset,
                                width: panel_bounds.width,
                                height,
                            }
                        };

                        let info = if mt.is_separator {
                            crate::core::accessibility::WidgetInfo::new(
                                crate::core::accessibility::Role::Splitter,
                            )
                            .with_bounds(bounds)
                        } else {
                            let label = mt
                                .item
                                .as_widget()
                                .accessibility_label()
                                .map(|s| s.into_owned())
                                .unwrap_or_default();

                            let mut wi = crate::core::accessibility::WidgetInfo::menu_item(label)
                                .with_bounds(bounds);

                            if !mt.children.is_empty() {
                                wi = wi.with_expanded(Some(
                                    state.active_root.get(panel_index + 1) == Some(&item_index),
                                ));
                            }

                            wi
                        };

                        operation.accessibility(Some(&item_id), bounds, info);
                    }
                }
            });
        }

        operation.leave_container();
    }
    #[allow(clippy::too_many_lines)]
    pub(super) fn on_event_inner(
        &mut self,
        event: &event::Event,
        layout: Layout<'_>,
        view_cursor: Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> event::Status {
        use event::{
            Event::{Keyboard, Mouse, Touch},
            Status::{Captured, Ignored},
        };
        use mouse::Event::{ButtonPressed, ButtonReleased, CursorMoved, WheelScrolled};
        use touch::Event::{FingerLifted, FingerMoved, FingerPressed};

        if !self
            .tree
            .inner
            .with_data(|data| data.open || !data.active_root.is_empty())
        {
            return Ignored;
        }

        let viewport = layout.bounds();
        let viewport_size = viewport.size();
        let overlay_offset = Point::ORIGIN - viewport.position();

        #[cfg(feature = "accessibility")]
        if let event::Event::Accessibility(accessibility_event) = event {
            use crate::core::accessibility::node_id_from_widget_id;

            // Update highlight based on accessibility focus/click/blur.
            // Keep this strictly state-local to avoid borrow conflicts.
            let Menu {
                tree, menu_roots, ..
            } = self;

            let mut needs_redraw = false;
            let status = tree.inner.with_data_mut(|state| {
                if !state.open || state.active_root.is_empty() {
                    return Ignored;
                }

                let Some(first_root) = state.active_root.first().copied() else {
                    return Ignored;
                };
                let Some(root_entry) = menu_roots.get(first_root) else {
                    return Ignored;
                };

                for panel_index in 0..state.menu_states.len() {
                    let menu_items: &[_] = state.active_root.iter().skip(1).take(panel_index).fold(
                        &root_entry.children[..],
                        |mt, &next_active_root| {
                            mt.get(next_active_root)
                                .map(|m| &m.children[..])
                                .unwrap_or(mt)
                        },
                    );

                    for item_index in 0..menu_items.len() {
                        let item_id = crate::core::widget::Id::from(format!(
                            "icy_ui.menu/{}/panel/{}/item/{}",
                            first_root, panel_index, item_index
                        ));
                        let node_id = node_id_from_widget_id(&item_id);

                        if node_id != accessibility_event.target {
                            continue;
                        }

                        // Ignore separators
                        if menu_items.get(item_index).is_some_and(|m| m.is_separator) {
                            return Ignored;
                        }

                        // Close any deeper menus first.
                        state.menu_states.truncate(panel_index + 1);
                        state.active_root.truncate(panel_index + 1);

                        let ms = &mut state.menu_states[panel_index];
                        if accessibility_event.is_blur() {
                            ms.index = None;
                        } else if accessibility_event.is_focus() || accessibility_event.is_click() {
                            ms.index = Some(item_index);
                        }

                        needs_redraw = true;
                        return Captured;
                    }
                }

                Ignored
            });

            if needs_redraw {
                shell.request_redraw();
            }

            return status;
        }

        // Handle keyboard events first
        if let Keyboard(_) = event {
            return self.handle_keyboard_event(
                event,
                renderer,
                clipboard,
                shell,
                overlay_offset,
                viewport_size,
            );
        }

        let overlay_cursor = view_cursor.position().unwrap_or_default() - overlay_offset;

        let menu_status = process_menu_events(
            self,
            event,
            view_cursor,
            renderer,
            clipboard,
            shell,
            overlay_offset,
        );

        init_root_menu(
            self,
            renderer,
            shell,
            overlay_cursor,
            viewport_size,
            overlay_offset,
            self.bar_bounds,
            self.main_offset as f32,
        );

        match event {
            Mouse(WheelScrolled { delta, .. }) => {
                process_scroll_events(self, *delta, overlay_cursor, viewport_size, overlay_offset)
                    .merge(menu_status)
            }

            Mouse(ButtonPressed {
                button: mouse::Button::Left,
                ..
            })
            | Touch(FingerPressed { .. }) => {
                self.tree.inner.with_data_mut(|data| {
                    data.pressed = true;
                    data.view_cursor = view_cursor;
                });
                Captured
            }

            Mouse(CursorMoved { position, .. }) | Touch(FingerMoved { position, .. }) => {
                let view_cursor = Cursor::Available(*position);
                let overlay_cursor = view_cursor.position().unwrap_or_default() - overlay_offset;
                if !self.is_overlay && !view_cursor.is_over(viewport) {
                    return menu_status;
                }

                process_overlay_events(
                    self,
                    renderer,
                    layout,
                    viewport_size,
                    overlay_offset,
                    view_cursor,
                    overlay_cursor,
                    self.cross_offset as f32,
                    shell,
                )
                .merge(menu_status)
            }

            Mouse(ButtonReleased { .. }) | Touch(FingerLifted { .. }) => {
                self.tree.inner.with_data_mut(|state| {
                    state.pressed = false;

                    if state
                        .view_cursor
                        .position()
                        .unwrap_or_default()
                        .distance(view_cursor.position().unwrap_or_default())
                        < 2.0
                    {
                        let is_inside = state
                            .menu_states
                            .iter()
                            .any(|ms| ms.menu_bounds.check_bounds.contains(overlay_cursor));

                        let mut needs_reset = false;
                        needs_reset |= self.close_condition.click_inside
                            && is_inside
                            && matches!(
                                event,
                                Mouse(ButtonReleased {
                                    button: mouse::Button::Left,
                                    ..
                                }) | Touch(FingerLifted { .. })
                            );
                        // Only close on Left-click outside (or touch).
                        // Right-click outside is handled by the ContextMenu
                        // widget which can properly close and reopen in one
                        // step, avoiding a race between the overlay and widget
                        // event loops.
                        needs_reset |= self.close_condition.click_outside
                            && !is_inside
                            && matches!(
                                event,
                                Mouse(ButtonReleased {
                                    button: mouse::Button::Left,
                                    ..
                                }) | Touch(FingerLifted { .. })
                            );

                        if needs_reset {
                            state.reset();
                            return Captured;
                        }
                    }

                    if self.bar_bounds.contains(overlay_cursor) {
                        state.reset();
                        Captured
                    } else {
                        menu_status
                    }
                })
            }

            _ => menu_status,
        }
    }

    /// Activate the currently selected menu item (called on Enter key)
    /// This is done by setting a flag that will be processed in the next mouse release event simulation
    fn activate_selected_item(
        &mut self,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        overlay_offset: Vector,
    ) {
        let menu_roots = &mut self.menu_roots;

        self.tree.inner.with_data_mut(|state| {
            if !state.open || state.menu_states.is_empty() || state.active_root.is_empty() {
                return;
            }

            let Some(hover) = state.menu_states.last() else {
                return;
            };

            let Some(hover_index) = hover.index else {
                return;
            };

            // Get the selected menu item
            let Some(first_root) = state.active_root.first().copied() else {
                return;
            };
            let Some(root_entry) = menu_roots.get_mut(first_root) else {
                return;
            };
            let mt = state
                .active_root
                .iter()
                .skip(1)
                .fold(root_entry, |mt, &next_active_root| {
                    if next_active_root < mt.children.len() {
                        &mut mt.children[next_active_root]
                    } else {
                        mt
                    }
                });

            let Some(mt) = mt.children.get_mut(hover_index) else {
                return;
            };

            // Skip if it's a separator
            if mt.is_separator {
                return;
            }

            // If it has children, it's a submenu - don't activate, just open
            if !mt.children.is_empty() {
                // TODO: Open submenu
                return;
            }

            // Get the layout for the selected item
            let Some(tree_entry) = state.tree.children.get_mut(first_root) else {
                return;
            };
            let Some(tree) = tree_entry.children.get_mut(mt.index) else {
                return;
            };

            let child_node = hover.layout_single(overlay_offset, hover_index, renderer, mt, tree);
            let child_layout = Layout::new(&child_node);

            // Create a cursor position inside the item bounds
            let bounds = child_layout.bounds();
            let center = bounds.center();
            let cursor = Cursor::Available(center);

            let modifiers = crate::core::keyboard::Modifiers::default();
            let press_event = event::Event::Mouse(mouse::Event::ButtonPressed {
                button: mouse::Button::Left,
                modifiers,
            });

            // Send press event
            mt.item.as_widget_mut().update(
                tree,
                &press_event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                &Rectangle::default(),
            );

            // Reset capture status so the release event can be processed
            shell.uncapture_event();

            let release_event = event::Event::Mouse(mouse::Event::ButtonReleased {
                button: mouse::Button::Left,
                modifiers,
            });

            // Send release event
            mt.item.as_widget_mut().update(
                tree,
                &release_event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                &Rectangle::default(),
            );

            // Close the menu after activation
            state.reset();
        });

        shell.request_redraw();
    }

    /// Open a root menu by index (for keyboard navigation)
    fn open_root_menu_by_index(
        &mut self,
        index: usize,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        viewport_size: Size,
        overlay_offset: Vector,
    ) {
        let menu_roots = &mut self.menu_roots;
        let root_bounds_list = &self.root_bounds_list;
        let item_width = self.item_width;
        let item_height = self.item_height;
        let bounds_expand = self.bounds_expand;
        let is_overlay = self.is_overlay;
        let main_offset = self.main_offset as f32;

        #[cfg(feature = "accessibility")]
        let mut a11y_focus_request: Option<crate::core::accessibility::NodeId> = None;

        self.tree.inner.with_data_mut(|state| {
            // Check if the index is valid
            if index >= menu_roots.len() || index >= root_bounds_list.len() {
                return;
            }

            let mt = &mut menu_roots[index];

            // Skip if this root has no children
            if mt.children.is_empty() {
                return;
            }

            let root_bounds = root_bounds_list[index];

            // Clear existing menu states
            state.menu_states.clear();
            state.active_root.clear();

            // Determine direction based on position (only for overlays)
            // For menu bars, keep the direction set by the parent (based on RTL)
            let view_center = viewport_size.width * 0.5;
            let rb_center = root_bounds.center_x();

            if is_overlay {
                let is_rtl = crate::core::layout_direction().is_rtl();
                state.horizontal_direction = if is_rtl {
                    if rb_center < view_center {
                        Direction::Positive
                    } else {
                        Direction::Negative
                    }
                } else {
                    if rb_center > view_center {
                        Direction::Negative
                    } else {
                        Direction::Positive
                    }
                };
            }

            let aod = Aod {
                horizontal: true,
                vertical: true,
                horizontal_overlap: true,
                vertical_overlap: false,
                horizontal_direction: state.horizontal_direction,
                vertical_direction: state.vertical_direction,
                horizontal_offset: 0.0,
                vertical_offset: main_offset,
            };

            let Some(tree_entry) = state.tree.children.get_mut(index) else {
                return;
            };
            let menu_bounds = MenuBounds::new(
                mt,
                renderer,
                item_width,
                item_height,
                viewport_size,
                overlay_offset,
                &aod,
                bounds_expand,
                root_bounds,
                &mut tree_entry.children,
                is_overlay,
            );

            let selected_index = mt.children.iter().position(|m| !m.is_separator);

            state.active_root.push(index);
            let ms = MenuState {
                index: selected_index,
                scroll_offset: 0.0,
                menu_bounds,
            };
            state.menu_states.push(ms);

            #[cfg(feature = "accessibility")]
            if let Some(item_index) = selected_index {
                let item_id = crate::core::widget::Id::from(format!(
                    "icy_ui.menu/{}/panel/{}/item/{}",
                    index, 0, item_index
                ));
                a11y_focus_request =
                    Some(crate::core::accessibility::node_id_from_widget_id(&item_id));
            }
        });

        #[cfg(feature = "accessibility")]
        if let Some(target) = a11y_focus_request {
            shell.request_a11y_focus(target);
        }

        shell.invalidate_layout();
        shell.request_redraw();
    }

    /// Handle keyboard navigation in the menu
    #[allow(clippy::too_many_lines)]
    fn handle_keyboard_event(
        &mut self,
        event: &event::Event,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        overlay_offset: Vector,
        viewport_size: Size,
    ) -> event::Status {
        use event::Status::{Captured, Ignored};
        use keyboard::key::Named;

        // While the menu overlay is open, it receives keyboard events first.
        // Therefore we must handle Alt *release* here too, otherwise mnemonic
        // underline toggling will not repaint while a submenu is shown.
        match event {
            event::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Alt),
                ..
            }) if mnemonics_enabled() => {
                self.tree.inner.with_data_mut(|state| {
                    state.alt_pressed = true;
                    state.show_mnemonics = true;
                });
                set_show_underlines(true);
                shell.invalidate_layout();
                shell.request_redraw();
                return Captured;
            }

            event::Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(Named::Alt),
                ..
            }) if mnemonics_enabled() => {
                self.tree.inner.with_data_mut(|state| {
                    state.alt_pressed = false;
                    state.show_mnemonics = false;
                });
                set_show_underlines(false);
                shell.invalidate_layout();
                shell.request_redraw();
                return Captured;
            }

            _ => {}
        }

        let event::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) = event else {
            return Ignored;
        };

        match key {
            keyboard::Key::Named(Named::Escape) => {
                self.tree.inner.with_data_mut(|state| {
                    if state.open {
                        state.reset();
                    }
                });
                shell.request_redraw();
                Captured
            }

            keyboard::Key::Named(Named::ArrowUp) => {
                // Get the active menu items to check for separators
                let menu_roots = &self.menu_roots;

                #[cfg(feature = "accessibility")]
                let mut a11y_focus_request: Option<
                    crate::core::accessibility::NodeId,
                > = None;

                self.tree.inner.with_data_mut(|state| {
                    if state.open && !state.menu_states.is_empty() && !state.active_root.is_empty()
                    {
                        // Get the current menu items
                        let Some(first_root) = state.active_root.first().copied() else {
                            return;
                        };
                        let Some(root_entry) = menu_roots.get(first_root) else {
                            return;
                        };
                        let active_menu = state
                            .active_root
                            .iter()
                            .skip(1)
                            .fold(&root_entry.children, |mt, next| {
                                mt.get(*next).map(|m| &m.children).unwrap_or(mt)
                            });

                        if let Some(ms) = state.menu_states.last_mut() {
                            let count = ms.menu_bounds.child_positions.len();
                            if count > 0 {
                                let mut new_index = match ms.index {
                                    Some(i) if i > 0 => i - 1,
                                    Some(_) => count - 1,
                                    None => count - 1,
                                };

                                // Skip separators (wrap around if needed)
                                let mut iterations = 0;
                                while iterations < count {
                                    if new_index < active_menu.len()
                                        && !active_menu
                                            .get(new_index)
                                            .map_or(true, |m| m.is_separator)
                                    {
                                        break;
                                    }
                                    new_index = if new_index == 0 {
                                        count - 1
                                    } else {
                                        new_index - 1
                                    };
                                    iterations += 1;
                                }

                                // Only update if we found a non-separator
                                if iterations < count {
                                    ms.index = Some(new_index);

                                    #[cfg(feature = "accessibility")]
                                    {
                                        let panel_index = state.menu_states.len() - 1;
                                        let item_id = crate::core::widget::Id::from(format!(
                                            "icy_ui.menu/{}/panel/{}/item/{}",
                                            first_root, panel_index, new_index
                                        ));
                                        a11y_focus_request = Some(
                                            crate::core::accessibility::node_id_from_widget_id(
                                                &item_id,
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                });

                #[cfg(feature = "accessibility")]
                if let Some(target) = a11y_focus_request {
                    shell.request_a11y_focus(target);
                }

                shell.request_redraw();
                Captured
            }

            keyboard::Key::Named(Named::ArrowDown) => {
                // Get the active menu items to check for separators
                let menu_roots = &self.menu_roots;

                #[cfg(feature = "accessibility")]
                let mut a11y_focus_request: Option<
                    crate::core::accessibility::NodeId,
                > = None;

                self.tree.inner.with_data_mut(|state| {
                    if state.open && !state.menu_states.is_empty() && !state.active_root.is_empty()
                    {
                        // Get the current menu items
                        let Some(first_root) = state.active_root.first().copied() else {
                            return;
                        };
                        let Some(root_entry) = menu_roots.get(first_root) else {
                            return;
                        };
                        let active_menu = state
                            .active_root
                            .iter()
                            .skip(1)
                            .fold(&root_entry.children, |mt, next| {
                                mt.get(*next).map(|m| &m.children).unwrap_or(mt)
                            });

                        if let Some(ms) = state.menu_states.last_mut() {
                            let count = ms.menu_bounds.child_positions.len();
                            if count > 0 {
                                let mut new_index = match ms.index {
                                    Some(i) if i < count - 1 => i + 1,
                                    Some(_) => 0,
                                    None => 0,
                                };

                                // Skip separators (wrap around if needed)
                                let mut iterations = 0;
                                while iterations < count {
                                    if new_index < active_menu.len()
                                        && !active_menu
                                            .get(new_index)
                                            .map_or(true, |m| m.is_separator)
                                    {
                                        break;
                                    }
                                    new_index = if new_index >= count - 1 {
                                        0
                                    } else {
                                        new_index + 1
                                    };
                                    iterations += 1;
                                }

                                // Only update if we found a non-separator
                                if iterations < count {
                                    ms.index = Some(new_index);

                                    #[cfg(feature = "accessibility")]
                                    {
                                        let panel_index = state.menu_states.len() - 1;
                                        let item_id = crate::core::widget::Id::from(format!(
                                            "icy_ui.menu/{}/panel/{}/item/{}",
                                            first_root, panel_index, new_index
                                        ));
                                        a11y_focus_request = Some(
                                            crate::core::accessibility::node_id_from_widget_id(
                                                &item_id,
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                });

                #[cfg(feature = "accessibility")]
                if let Some(target) = a11y_focus_request {
                    shell.request_a11y_focus(target);
                }

                shell.request_redraw();
                Captured
            }

            keyboard::Key::Named(Named::ArrowLeft) => {
                // First check if we're in a submenu - if so, go back to parent
                let in_submenu = self
                    .tree
                    .inner
                    .with_data(|state| state.open && state.menu_states.len() > 1);

                if in_submenu {
                    #[cfg(feature = "accessibility")]
                    let mut a11y_focus_request: Option<
                        crate::core::accessibility::NodeId,
                    > = None;

                    self.tree.inner.with_data_mut(|state| {
                        // Pop the submenu panel and restore selection to the item that opened it.
                        let last_opening_index = state.active_root.pop();
                        let _ = state.menu_states.pop();

                        if let (Some(opening_index), Some(parent_state)) =
                            (last_opening_index, state.menu_states.last_mut())
                        {
                            parent_state.index = Some(opening_index);
                        }

                        #[cfg(feature = "accessibility")]
                        {
                            let Some(first_root) = state.active_root.first().copied() else {
                                return;
                            };
                            let Some(panel_index) = state.menu_states.len().checked_sub(1) else {
                                return;
                            };
                            let Some(item_index) = state.menu_states.last().and_then(|ms| ms.index)
                            else {
                                return;
                            };

                            let item_id = crate::core::widget::Id::from(format!(
                                "icy_ui.menu/{}/panel/{}/item/{}",
                                first_root, panel_index, item_index
                            ));
                            a11y_focus_request =
                                Some(crate::core::accessibility::node_id_from_widget_id(&item_id));
                        }
                    });

                    #[cfg(feature = "accessibility")]
                    if let Some(target) = a11y_focus_request {
                        shell.request_a11y_focus(target);
                    }

                    shell.invalidate_layout();
                    shell.request_redraw();
                    Captured
                } else {
                    // Switch to previous root menu
                    let current_root = self.tree.inner.with_data(|state| {
                        if state.open && !state.active_root.is_empty() {
                            state.active_root.first().copied()
                        } else {
                            None
                        }
                    });

                    if let Some(current) = current_root {
                        let root_count = self.menu_roots.len();
                        // Find previous root that has children
                        let mut new_root = if current == 0 {
                            root_count - 1
                        } else {
                            current - 1
                        };
                        let mut iterations = 0;
                        while iterations < root_count {
                            if !self.menu_roots[new_root].children.is_empty() {
                                break;
                            }
                            new_root = if new_root == 0 {
                                root_count - 1
                            } else {
                                new_root - 1
                            };
                            iterations += 1;
                        }

                        if iterations < root_count && new_root != current {
                            self.open_root_menu_by_index(
                                new_root,
                                renderer,
                                shell,
                                viewport_size,
                                overlay_offset,
                            );
                        }
                        Captured
                    } else {
                        Ignored
                    }
                }
            }

            keyboard::Key::Named(Named::ArrowRight) => {
                // If current item has a submenu: open it and select its first item.
                // Otherwise, switch to the next root menu (menubar behavior).

                #[cfg(feature = "accessibility")]
                let mut a11y_focus_request: Option<
                    crate::core::accessibility::NodeId,
                > = None;

                let mut opened_submenu = false;

                {
                    let menu_roots = &mut self.menu_roots;
                    let item_width = self.item_width;
                    let item_height = self.item_height;
                    let bounds_expand = self.bounds_expand;
                    let is_overlay = self.is_overlay;
                    let cross_offset = self.cross_offset as f32;

                    self.tree.inner.with_data_mut(|state| {
                        if !state.open
                            || state.menu_states.is_empty()
                            || state.active_root.is_empty()
                        {
                            return;
                        }

                        let Some(first_root) = state.active_root.first().copied() else {
                            return;
                        };

                        let Some(root_entry) = menu_roots.get_mut(first_root) else {
                            return;
                        };

                        let Some(tree_entry) = state.tree.children.get_mut(first_root) else {
                            return;
                        };

                        let Some(last_menu_state) = state.menu_states.last_mut() else {
                            return;
                        };
                        let Some(hover_index) = last_menu_state.index else {
                            return;
                        };

                        // Current menu list (based on active_root path)
                        let active_menu = state.active_root.iter().skip(1).fold(
                            &mut root_entry.children,
                            |mt, &next| {
                                if next < mt.len() {
                                    &mut mt[next].children
                                } else {
                                    mt
                                }
                            },
                        );

                        let Some(item) = active_menu.get_mut(hover_index) else {
                            return;
                        };

                        if item.children.is_empty() {
                            return;
                        }

                        let last_menu_bounds = &last_menu_state.menu_bounds;
                        let Some(&item_pos) = last_menu_bounds.child_positions.get(hover_index)
                        else {
                            return;
                        };
                        let Some(&item_size) = last_menu_bounds.child_sizes.get(hover_index) else {
                            return;
                        };

                        let item_position =
                            Point::new(0.0, item_pos + last_menu_state.scroll_offset);
                        let item_bounds = Rectangle::new(item_position, item_size)
                            + (last_menu_bounds.children_bounds.position() - Point::ORIGIN);

                        let aod = Aod {
                            horizontal: true,
                            vertical: true,
                            horizontal_overlap: false,
                            vertical_overlap: true,
                            horizontal_direction: state.horizontal_direction,
                            vertical_direction: state.vertical_direction,
                            horizontal_offset: cross_offset,
                            vertical_offset: 0.0,
                        };

                        let menu_bounds = MenuBounds::new(
                            item,
                            renderer,
                            item_width,
                            item_height,
                            viewport_size,
                            overlay_offset,
                            &aod,
                            bounds_expand,
                            item_bounds,
                            &mut tree_entry.children,
                            is_overlay,
                        );

                        let selected_index = item.children.iter().position(|m| !m.is_separator);

                        if is_overlay {
                            state.active_root.push(hover_index);
                        } else {
                            state.menu_states.truncate(1);
                        }

                        state.menu_states.push(MenuState {
                            index: selected_index,
                            scroll_offset: 0.0,
                            menu_bounds,
                        });

                        opened_submenu = true;

                        #[cfg(feature = "accessibility")]
                        {
                            if let Some(item_index) = selected_index {
                                let panel_index = state.menu_states.len() - 1;
                                let item_id = crate::core::widget::Id::from(format!(
                                    "icy_ui.menu/{}/panel/{}/item/{}",
                                    first_root, panel_index, item_index
                                ));
                                a11y_focus_request = Some(
                                    crate::core::accessibility::node_id_from_widget_id(&item_id),
                                );
                            }
                        }
                    });
                }

                if opened_submenu {
                    #[cfg(feature = "accessibility")]
                    if let Some(target) = a11y_focus_request {
                        shell.request_a11y_focus(target);
                    }
                    shell.invalidate_layout();
                    shell.request_redraw();
                    Captured
                } else {
                    // Switch to next root menu
                    let current_root = self.tree.inner.with_data(|state| {
                        if state.open && !state.active_root.is_empty() {
                            state.active_root.first().copied()
                        } else {
                            None
                        }
                    });

                    if let Some(current) = current_root {
                        let root_count = self.menu_roots.len();
                        // Find next root that has children
                        let mut new_root = if current >= root_count - 1 {
                            0
                        } else {
                            current + 1
                        };
                        let mut iterations = 0;
                        while iterations < root_count {
                            if !self.menu_roots[new_root].children.is_empty() {
                                break;
                            }
                            new_root = if new_root >= root_count - 1 {
                                0
                            } else {
                                new_root + 1
                            };
                            iterations += 1;
                        }

                        if iterations < root_count && new_root != current {
                            self.open_root_menu_by_index(
                                new_root,
                                renderer,
                                shell,
                                viewport_size,
                                overlay_offset,
                            );
                        }
                        Captured
                    } else {
                        Ignored
                    }
                }
            }

            keyboard::Key::Named(Named::Enter) => {
                // Activate the currently selected item by simulating a click
                self.activate_selected_item(renderer, clipboard, shell, overlay_offset);
                Captured
            }

            keyboard::Key::Named(Named::Home) => {
                // Select the first non-separator item
                let menu_roots = &self.menu_roots;

                #[cfg(feature = "accessibility")]
                let mut a11y_focus_request: Option<
                    crate::core::accessibility::NodeId,
                > = None;

                self.tree.inner.with_data_mut(|state| {
                    if state.open && !state.menu_states.is_empty() && !state.active_root.is_empty()
                    {
                        let Some(first_root) = state.active_root.first().copied() else {
                            return;
                        };
                        let Some(root_entry) = menu_roots.get(first_root) else {
                            return;
                        };
                        let active_menu = state
                            .active_root
                            .iter()
                            .skip(1)
                            .fold(&root_entry.children, |mt, next| {
                                mt.get(*next).map(|m| &m.children).unwrap_or(mt)
                            });

                        if let Some(ms) = state.menu_states.last_mut() {
                            let count = ms.menu_bounds.child_positions.len();
                            // Find first non-separator
                            for i in 0..count {
                                if i < active_menu.len() && !active_menu[i].is_separator {
                                    ms.index = Some(i);

                                    #[cfg(feature = "accessibility")]
                                    {
                                        let panel_index = state.menu_states.len() - 1;
                                        let item_id = crate::core::widget::Id::from(format!(
                                            "icy_ui.menu/{}/panel/{}/item/{}",
                                            first_root, panel_index, i
                                        ));
                                        a11y_focus_request = Some(
                                            crate::core::accessibility::node_id_from_widget_id(
                                                &item_id,
                                            ),
                                        );
                                    }
                                    break;
                                }
                            }
                        }
                    }
                });

                #[cfg(feature = "accessibility")]
                if let Some(target) = a11y_focus_request {
                    shell.request_a11y_focus(target);
                }

                shell.request_redraw();
                Captured
            }

            keyboard::Key::Named(Named::End) => {
                // Select the last non-separator item
                let menu_roots = &self.menu_roots;

                #[cfg(feature = "accessibility")]
                let mut a11y_focus_request: Option<
                    crate::core::accessibility::NodeId,
                > = None;

                self.tree.inner.with_data_mut(|state| {
                    if state.open && !state.menu_states.is_empty() && !state.active_root.is_empty()
                    {
                        let Some(first_root) = state.active_root.first().copied() else {
                            return;
                        };
                        let Some(root_entry) = menu_roots.get(first_root) else {
                            return;
                        };
                        let active_menu = state
                            .active_root
                            .iter()
                            .skip(1)
                            .fold(&root_entry.children, |mt, next| {
                                mt.get(*next).map(|m| &m.children).unwrap_or(mt)
                            });

                        if let Some(ms) = state.menu_states.last_mut() {
                            let count = ms.menu_bounds.child_positions.len();
                            // Find last non-separator
                            for i in (0..count).rev() {
                                if i < active_menu.len() && !active_menu[i].is_separator {
                                    ms.index = Some(i);

                                    #[cfg(feature = "accessibility")]
                                    {
                                        let panel_index = state.menu_states.len() - 1;
                                        let item_id = crate::core::widget::Id::from(format!(
                                            "icy_ui.menu/{}/panel/{}/item/{}",
                                            first_root, panel_index, i
                                        ));
                                        a11y_focus_request = Some(
                                            crate::core::accessibility::node_id_from_widget_id(
                                                &item_id,
                                            ),
                                        );
                                    }
                                    break;
                                }
                            }
                        }
                    }
                });

                #[cfg(feature = "accessibility")]
                if let Some(target) = a11y_focus_request {
                    shell.request_a11y_focus(target);
                }

                shell.request_redraw();
                Captured
            }

            // Letter key for mnemonic navigation in open menus (no modifiers required)
            keyboard::Key::Character(c) if mnemonics_enabled() => {
                let char_lower = c.chars().next().map(|ch| ch.to_ascii_lowercase());

                if let Some(ch) = char_lower {
                    let menu_roots = &self.menu_roots;
                    let mut captured = false;

                    self.tree.inner.with_data_mut(|state| {
                        if state.open && !state.active_root.is_empty() {
                            // Get the current menu items
                            let Some(first_root) = state.active_root.first().copied() else {
                                return;
                            };
                            let Some(root_entry) = menu_roots.get(first_root) else {
                                return;
                            };
                            let active_menu = state
                                .active_root
                                .iter()
                                .skip(1)
                                .fold(&root_entry.children, |mt, next| {
                                    mt.get(*next).map(|m| &m.children).unwrap_or(mt)
                                });

                            // Find item with matching mnemonic
                            for (idx, item) in active_menu.iter().enumerate() {
                                if item.mnemonic == Some(ch) && !item.is_separator {
                                    if let Some(ms) = state.menu_states.last_mut() {
                                        ms.index = Some(idx);
                                    }
                                    captured = true;
                                    break;
                                }
                            }
                        }
                    });

                    if captured {
                        // Activate the selected item
                        self.activate_selected_item(renderer, clipboard, shell, overlay_offset);
                        return Captured;
                    }
                }
                Ignored
            }

            _ => Ignored,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        view_cursor: Cursor,
    ) {
        self.tree.inner.with_data(|state| {
            if !state.open || state.active_root.is_empty() {
                return;
            }

            let active_root = &state.active_root;
            let viewport = layout.bounds();
            let viewport_size = viewport.size();
            let overlay_offset = Point::ORIGIN - viewport.position();

            let render_bounds = if self.is_overlay {
                Rectangle::new(Point::ORIGIN, viewport.size())
            } else {
                Rectangle::new(Point::ORIGIN, Size::INFINITE)
            };

            let styling = theme.appearance(&self.style);
            let Some(first_root) = active_root.first().copied() else {
                return;
            };
            let Some(root_entry) = self.menu_roots.get(first_root) else {
                return;
            };

            let indices = state.get_trimmed_indices(0).collect::<Vec<_>>();

            for (i, (ms, children_layout)) in
                state.menu_states.iter().zip(layout.children()).enumerate()
            {
                if !self.is_overlay && i > 0 {
                    continue;
                }

                let draw_path = self.path_highlight.as_ref().is_some_and(|ph| match ph {
                    PathHighlight::Full => true,
                    PathHighlight::OmitActive => !indices.is_empty() && i < indices.len() - 1,
                    // Keep the whole open path highlighted (VS Code-style):
                    // when a submenu is open, the item that opened it should remain selected.
                    PathHighlight::MenuActive => !indices.is_empty() && i < indices.len(),
                });

                let view_cursor = if i == state.menu_states.len() - 1 {
                    view_cursor
                } else {
                    Cursor::Available([-1.0; 2].into())
                };

                renderer.with_layer(render_bounds, |r| {
                    // Each menu panel draws a different slice of the `MenuTree` hierarchy.
                    // Depth 0 draws `root_entry.children`, depth 1 draws the children of the
                    // active item in depth 0, etc.
                    let menu_items = active_root.iter().skip(1).take(i).fold(
                        &root_entry.children[..],
                        |mt, &next_active_root| {
                            mt.get(next_active_root)
                                .map(|m| &m.children[..])
                                .unwrap_or(mt)
                        },
                    );

                    let slice = ms.slice(viewport_size, overlay_offset, self.item_height);
                    let start_index = slice.start_index;
                    let end_index = slice.end_index;

                    let children_bounds = children_layout.bounds();

                    // Convert background_expand from [u16; 4] to Padding
                    let [exp_top, exp_right, exp_bottom, exp_left] = styling.background_expand;
                    let bg_padding = crate::core::Padding {
                        top: exp_top as f32,
                        right: exp_right as f32,
                        bottom: exp_bottom as f32,
                        left: exp_left as f32,
                    };

                    // Convert menu_border_radius from [f32; 4] to Radius
                    let [tl, tr, br, bl] = styling.menu_border_radius;
                    let menu_radius = crate::core::border::Radius {
                        top_left: tl,
                        top_right: tr,
                        bottom_right: br,
                        bottom_left: bl,
                    };

                    // draw menu background
                    let menu_quad = renderer::Quad {
                        bounds: pad_rectangle(
                            children_bounds.intersection(&viewport).unwrap_or_default(),
                            bg_padding,
                        ),
                        border: Border {
                            radius: menu_radius,
                            width: styling.border_width,
                            color: styling.border_color,
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    };
                    r.fill_quad(menu_quad, styling.background);

                    // draw path highlight
                    if let (true, Some(active)) = (draw_path, ms.index) {
                        if let Some(active_layout) = children_layout
                            .children()
                            .nth(active.saturating_sub(start_index))
                        {
                            // Convert path_border_radius from [f32; 4] to Radius
                            let [ptl, ptr, pbr, pbl] = styling.path_border_radius;
                            let path_radius = crate::core::border::Radius {
                                top_left: ptl,
                                top_right: ptr,
                                bottom_right: pbr,
                                bottom_left: pbl,
                            };

                            // Shrink the highlight bounds by the popup menu content padding
                            let [p_top, p_right, p_bottom, p_left] =
                                styling.menu_inner_content_padding;
                            let item_bounds = active_layout.bounds();
                            let highlight_bounds = Rectangle {
                                x: item_bounds.x + p_left,
                                y: item_bounds.y + p_top,
                                width: item_bounds.width - p_left - p_right,
                                height: item_bounds.height - p_top - p_bottom,
                            };

                            let path_quad = renderer::Quad {
                                bounds: highlight_bounds
                                    .intersection(&viewport)
                                    .unwrap_or_default(),
                                border: Border {
                                    radius: path_radius,
                                    ..Default::default()
                                },
                                shadow: Shadow::default(),
                                snap: true,
                            };

                            r.fill_quad(path_quad, styling.path);
                        }
                    }

                    // draw items
                    if start_index < menu_items.len() {
                        if let Some(tree_entry) = state.tree.children.get(first_root) {
                            for (mt, clo) in menu_items
                                .get(start_index..=end_index)
                                .unwrap_or(&[])
                                .iter()
                                .zip(children_layout.children())
                            {
                                if let Some(tree_child) = tree_entry.children.get(mt.index) {
                                    mt.item.as_widget().draw(
                                        tree_child,
                                        r,
                                        theme,
                                        style,
                                        clo,
                                        view_cursor,
                                        &children_bounds
                                            .intersection(&viewport)
                                            .unwrap_or_default(),
                                    );
                                }
                            }
                        }
                    }
                });
            }
        });
    }
}
