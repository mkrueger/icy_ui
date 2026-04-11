//! Native macOS application menu bar and context menus.
//!
//! This module renders an [`icy_ui_core::menu::AppMenu`] as a native macOS
//! menu bar (`NSMenu`) and emits activated menu item identifiers.
//! It also provides support for native context menus.

use std::sync::mpsc::{Receiver, Sender};

use objc2::DefinedClass;
use objc2::define_class;
use objc2::rc::Retained;
use objc2::{msg_send, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem, NSView};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSString};

use icy_ui_core::keyboard::{Key, Modifiers, key::Named};
use icy_ui_core::menu::{
    AppMenu, ContextMenuItem, ContextMenuItemKind, MenuId, MenuKind, MenuNode, MenuRole,
    MenuShortcut,
};

/// Errors that can occur while installing or updating the macOS menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuError {
    /// Not running on the main thread (required for AppKit operations).
    NotMainThread,
}

impl std::fmt::Display for MenuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MenuError::NotMainThread => write!(f, "Not running on the main thread"),
        }
    }
}

impl std::error::Error for MenuError {}

/// A native macOS application menu.
///
/// The menu is installed as the `NSApplication` main menu.
pub struct MacMenu {
    _target: Retained<MenuTarget>,
    main_menu: Option<Retained<NSMenu>>,
    receiver: Receiver<MenuId>,
    last_signature: u64,
}

impl MacMenu {
    /// Creates a new [`MacMenu`].
    pub fn new() -> Result<Self, MenuError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(MenuError::NotMainThread);
        };

        let (sender, receiver) = std::sync::mpsc::channel();
        let target = MenuTarget::new(mtm, sender);

        Ok(Self {
            _target: target,
            main_menu: None,
            receiver,
            last_signature: 0,
        })
    }

    /// Updates the installed menu if the model has changed.
    pub fn sync<Message>(&mut self, menu: &AppMenu<Message>) -> Result<(), MenuError> {
        let signature = menu_signature(menu);

        if signature == self.last_signature {
            return Ok(());
        }

        let Some(mtm) = MainThreadMarker::new() else {
            return Err(MenuError::NotMainThread);
        };

        let ns_menu = build_main_menu(mtm, &self._target, menu);

        // Keep the menu alive for the lifetime of the app.
        self.main_menu = Some(ns_menu);

        let app = NSApplication::sharedApplication(mtm);
        if let Some(menu) = self.main_menu.as_deref() {
            app.setMainMenu(Some(menu));
        }

        self.last_signature = signature;

        Ok(())
    }

    /// Try to receive an activated menu id without blocking.
    pub fn try_recv(&self) -> Option<MenuId> {
        self.receiver.try_recv().ok()
    }
}

/// A native macOS context menu.
///
/// Context menus are displayed at a specific location in a view and
/// disappear after the user selects an item or clicks outside.
pub struct MacContextMenu {
    target: Retained<MenuTarget>,
    receiver: Receiver<MenuId>,
}

impl MacContextMenu {
    /// Creates a new [`MacContextMenu`].
    pub fn new() -> Result<Self, MenuError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(MenuError::NotMainThread);
        };

        let (sender, receiver) = std::sync::mpsc::channel();
        let target = MenuTarget::new(mtm, sender);

        Ok(Self { target, receiver })
    }

    /// Shows a context menu at the specified position.
    ///
    /// The position is in window coordinates (origin at bottom-left).
    /// The menu will be displayed and this function returns immediately.
    /// Use `try_recv` to get the selected menu item.
    ///
    /// # Arguments
    /// * `nodes` - The menu items to display
    /// * `view` - The NSView to display the menu in (as a raw pointer)
    /// * `x` - X position in window coordinates
    /// * `y` - Y position in window coordinates
    ///
    /// # Safety
    /// The `view_ptr` must be a valid pointer to an `NSView`.
    #[allow(unsafe_code)]
    pub unsafe fn show<Message>(
        &self,
        nodes: &[MenuNode<Message>],
        view_ptr: *mut std::ffi::c_void,
        x: f64,
        y: f64,
    ) -> Result<(), MenuError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(MenuError::NotMainThread);
        };

        let menu = build_context_menu(mtm, &self.target, nodes);
        let location = NSPoint::new(x, y);

        // Convert raw pointer to NSView reference
        // SAFETY: Caller guarantees view_ptr is valid
        let view: &NSView = unsafe { &*(view_ptr as *const NSView) };

        // Show the context menu
        // SAFETY: Objective-C message send
        // popUpMenuPositioningItem:atLocation:inView: returns BOOL
        let _: bool = msg_send![
            &menu,
            popUpMenuPositioningItem: std::ptr::null::<NSMenuItem>(),
            atLocation: location,
            inView: view
        ];

        Ok(())
    }

    /// Try to receive an activated menu id without blocking.
    pub fn try_recv(&self) -> Option<MenuId> {
        self.receiver.try_recv().ok()
    }

    /// Shows a context menu from [`ContextMenuItem`]s at the specified position.
    ///
    /// This is the preferred method when using the action-based context menu API.
    /// The menu blocks until the user makes a selection or dismisses it.
    /// Use `try_recv` to get the selected menu item.
    ///
    /// # Arguments
    /// * `items` - The menu items to display
    /// * `view_ptr` - The NSView to display the menu in (as a raw pointer)
    /// * `x` - X position in top-left origin coordinates (as used by iced)
    /// * `y` - Y position in top-left origin coordinates (as used by iced)
    ///
    /// Note: The function converts these coordinates to macOS bottom-left origin internally.
    ///
    /// # Safety
    /// The `view_ptr` must be a valid pointer to an `NSView`.
    #[allow(unsafe_code)]
    pub unsafe fn show_items(
        &self,
        items: &[ContextMenuItem],
        view_ptr: *mut std::ffi::c_void,
        x: f64,
        y: f64,
    ) -> Result<(), MenuError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(MenuError::NotMainThread);
        };

        let menu = build_context_menu_from_items(mtm, &self.target, items);

        // Convert raw pointer to NSView reference
        // SAFETY: Caller guarantees view_ptr is valid
        let view: &NSView = unsafe { &*(view_ptr as *const NSView) };

        // popUpMenuPositioningItem:atLocation:inView: uses the view's
        // coordinate system.  Winit's WinitView overrides isFlipped → true,
        // so the origin is already top-left (matching iced).  Only flip when
        // the view is *not* flipped (default AppKit bottom-left origin).
        let location = if view.isFlipped() {
            NSPoint::new(x, y)
        } else {
            let bounds = view.bounds();
            NSPoint::new(x, bounds.size.height - y)
        };

        // Show the context menu
        // SAFETY: Objective-C message send
        // popUpMenuPositioningItem:atLocation:inView: returns BOOL
        let _: bool = msg_send![
            &menu,
            popUpMenuPositioningItem: std::ptr::null::<NSMenuItem>(),
            atLocation: location,
            inView: view
        ];

        Ok(())
    }

    /// Shows a context menu from [`ContextMenuItem`]s and returns the selected menu ID.
    ///
    /// This is a convenience method that combines `show_items` and `try_recv`.
    /// The menu blocks until the user makes a selection or dismisses it.
    ///
    /// # Arguments
    /// * `items` - The menu items to display
    /// * `view_ptr` - The NSView to display the menu in (as a raw pointer)
    /// * `x` - X position in top-left origin coordinates (as used by iced)
    /// * `y` - Y position in top-left origin coordinates (as used by iced)
    ///
    /// Note: The function converts these coordinates to macOS bottom-left origin internally.
    ///
    /// # Returns
    /// `Some(MenuId)` if the user selected an item, `None` if dismissed.
    ///
    /// # Safety
    /// The `view_ptr` must be a valid pointer to an `NSView`.
    #[allow(unsafe_code)]
    pub unsafe fn show_items_and_wait(
        &self,
        items: &[ContextMenuItem],
        view_ptr: *mut std::ffi::c_void,
        x: f64,
        y: f64,
    ) -> Option<MenuId> {
        // SAFETY: Caller guarantees view_ptr is valid, delegating to show_items
        if unsafe { self.show_items(items, view_ptr, x, y) }.is_err() {
            return None;
        }
        self.try_recv()
    }
}

impl Default for MacContextMenu {
    fn default() -> Self {
        Self::new().expect("Failed to create context menu - not on main thread")
    }
}

/// Strips mnemonic markers ('&') from a label for display on macOS.
///
/// On Windows/Linux, '&' is used to mark keyboard mnemonics (e.g., "&File" shows as "File"
/// with 'F' underlined). macOS doesn't use this convention, so we strip the markers.
fn strip_mnemonic(label: &str) -> String {
    let mut result = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '&' {
            // '&&' becomes a single '&', otherwise skip the '&'
            if chars.peek() == Some(&'&') {
                result.push('&');
                let _ = chars.next();
            }
            // If next char exists and is not '&', we just skip this '&'
            // and include the next char normally (handled by the loop)
        } else {
            result.push(c);
        }
    }

    result
}

/// Builds a context menu from menu nodes.
fn build_context_menu<Message>(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    nodes: &[MenuNode<Message>],
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);

    for node in nodes {
        match &node.kind {
            MenuKind::Separator => {
                let sep = NSMenuItem::separatorItem(mtm);
                menu.addItem(&sep);
            }
            MenuKind::Submenu {
                label,
                enabled,
                children,
            } => {
                let item = NSMenuItem::new(mtm);
                item.setTitle(&NSString::from_str(&strip_mnemonic(label)));
                item.setEnabled(*enabled);

                let sub = build_context_menu(mtm, target, children);
                item.setSubmenu(Some(&sub));

                menu.addItem(&item);
            }
            MenuKind::Item {
                label,
                enabled,
                shortcut,
                on_activate: _,
            } => {
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    None,
                    shortcut.as_ref(),
                );
                menu.addItem(&item);
            }
            MenuKind::CheckItem {
                label,
                enabled,
                checked,
                shortcut,
                on_activate: _,
            } => {
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    *checked,
                    shortcut.as_ref(),
                );
                menu.addItem(&item);
            }
        }
    }

    menu
}

/// Builds a context menu from [`ContextMenuItem`]s.
///
/// Unlike `build_context_menu`, this function works with the simplified
/// `ContextMenuItem` type that doesn't have generic message parameters.
fn build_context_menu_from_items(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    items: &[ContextMenuItem],
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);

    for item in items {
        match &item.kind {
            ContextMenuItemKind::Separator => {
                let sep = NSMenuItem::separatorItem(mtm);
                menu.addItem(&sep);
            }
            ContextMenuItemKind::Submenu { label, children } => {
                let ns_item = NSMenuItem::new(mtm);
                ns_item.setTitle(&NSString::from_str(&strip_mnemonic(label)));
                ns_item.setEnabled(true);

                let sub = build_context_menu_from_items(mtm, target, children);
                ns_item.setSubmenu(Some(&sub));

                menu.addItem(&ns_item);
            }
            ContextMenuItemKind::Item { label, enabled } => {
                // Context menu items don't have shortcuts yet
                let ns_item = build_leaf_item(mtm, target, &item.id, label, *enabled, None, None);
                menu.addItem(&ns_item);
            }
            ContextMenuItemKind::CheckItem {
                label,
                enabled,
                checked,
            } => {
                // Context menu items don't have shortcuts yet
                let ns_item =
                    build_leaf_item(mtm, target, &item.id, label, *enabled, *checked, None);
                menu.addItem(&ns_item);
            }
        }
    }

    menu
}

struct MenuTargetIvars {
    sender: Sender<MenuId>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "IcyUiMenuTarget"]
    #[ivars = MenuTargetIvars]
    /// Objective-C target that forwards menu activations to a Rust channel.
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(menuItemActivated:))]
        #[allow(non_snake_case)]
        fn menuItemActivated_(&self, item: &NSMenuItem) {
            // SAFETY: Objective-C message sends.
            #[allow(unsafe_code)]
            unsafe {
                // Get the tag which stores the MenuId as u64
                let tag: isize = msg_send![item, tag];
                let _ = self.ivars().sender.send(MenuId::from_u64(tag as u64));
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, sender: Sender<MenuId>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(MenuTargetIvars { sender });

        // SAFETY: calling inherited init.
        #[allow(unsafe_code)]
        unsafe {
            msg_send![super(this), init]
        }
    }
}

fn build_main_menu<Message>(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    menu: &AppMenu<Message>,
) -> Retained<NSMenu> {
    let ns_menu = NSMenu::new(mtm);

    // Collect items with special roles from the entire menu tree.
    // These will be relocated to the application menu.
    let mut about_item: Option<&MenuNode<Message>> = None;
    let mut preferences_item: Option<&MenuNode<Message>> = None;
    let mut quit_item: Option<&MenuNode<Message>> = None;
    let mut app_specific_items: Vec<&MenuNode<Message>> = Vec::new();

    collect_role_items(
        &menu.roots,
        &mut about_item,
        &mut preferences_item,
        &mut quit_item,
        &mut app_specific_items,
    );

    // On macOS, the first top-level menu is treated as the "application" menu
    // and gets renamed to the app name. We build this menu with role-based items.
    {
        let app_item = NSMenuItem::new(mtm);
        app_item.setTitle(&NSString::from_str("Application"));

        let app_submenu = NSMenu::new(mtm);

        // Standard macOS application menu order:
        // 1. About
        // 2. ---
        // 3. Preferences (⌘,)
        // 4. ---
        // ...
        // N. Quit (⌘Q)

        if let Some(node) = about_item {
            if let MenuKind::Item {
                label,
                enabled,
                shortcut,
                ..
            } = &node.kind
            {
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    None,
                    shortcut.as_ref(),
                );
                app_submenu.addItem(&item);
                app_submenu.addItem(&NSMenuItem::separatorItem(mtm));
            }
        }

        if let Some(node) = preferences_item {
            if let MenuKind::Item {
                label,
                enabled,
                shortcut,
                ..
            } = &node.kind
            {
                // Use provided shortcut, or default to ⌘,
                let default_shortcut = MenuShortcut::cmd(Key::Character(",".into()));
                let shortcut_to_use = shortcut.as_ref().unwrap_or(&default_shortcut);
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    None,
                    Some(shortcut_to_use),
                );
                app_submenu.addItem(&item);
            }
        }

        // Add application-specific items (between Preferences and Quit).
        if !app_specific_items.is_empty() {
            app_submenu.addItem(&NSMenuItem::separatorItem(mtm));
            for node in &app_specific_items {
                if let MenuKind::Item {
                    label,
                    enabled,
                    shortcut,
                    ..
                } = &node.kind
                {
                    let item = build_leaf_item(
                        mtm,
                        target,
                        &node.id,
                        label,
                        *enabled,
                        None,
                        shortcut.as_ref(),
                    );
                    app_submenu.addItem(&item);
                }
            }
        }

        // Separator before Quit.
        if preferences_item.is_some() || !app_specific_items.is_empty() {
            app_submenu.addItem(&NSMenuItem::separatorItem(mtm));
        }

        if let Some(node) = quit_item {
            if let MenuKind::Item {
                label,
                enabled,
                shortcut,
                ..
            } = &node.kind
            {
                // Use provided shortcut, or default to ⌘Q
                let default_shortcut = MenuShortcut::cmd(Key::Character("q".into()));
                let shortcut_to_use = shortcut.as_ref().unwrap_or(&default_shortcut);
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    None,
                    Some(shortcut_to_use),
                );
                app_submenu.addItem(&item);
            }
        }

        app_item.setSubmenu(Some(&app_submenu));
        ns_menu.addItem(&app_item);
    }

    for root in &menu.roots {
        // macOS main menu expects top-level items with submenus.
        if let MenuKind::Submenu {
            label, children, ..
        } = &root.kind
        {
            // Filter out role items from children (recursively).
            let filtered: Vec<_> = children
                .iter()
                .filter(|n| n.role == MenuRole::None && !n.is_empty_submenu())
                .collect();

            // Skip empty menus (all items were relocated to app menu).
            if filtered.is_empty() {
                continue;
            }

            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(&strip_mnemonic(label)));

            let submenu = build_submenu_filtered(mtm, target, &filtered);
            item.setSubmenu(Some(&submenu));

            ns_menu.addItem(&item);
        }
    }

    ns_menu
}

/// Recursively collect items with special roles from the menu tree.
fn collect_role_items<'a, Message>(
    nodes: &'a [MenuNode<Message>],
    about: &mut Option<&'a MenuNode<Message>>,
    preferences: &mut Option<&'a MenuNode<Message>>,
    quit: &mut Option<&'a MenuNode<Message>>,
    app_specific: &mut Vec<&'a MenuNode<Message>>,
) {
    for node in nodes {
        match node.role {
            MenuRole::About if about.is_none() => *about = Some(node),
            MenuRole::Preferences if preferences.is_none() => *preferences = Some(node),
            MenuRole::Quit if quit.is_none() => *quit = Some(node),
            MenuRole::ApplicationSpecific => app_specific.push(node),
            MenuRole::None | _ => {}
        }

        if let MenuKind::Submenu { children, .. } = &node.kind {
            collect_role_items(children, about, preferences, quit, app_specific);
        }
    }
}

fn build_submenu<Message>(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    children: &[MenuNode<Message>],
) -> Retained<NSMenu> {
    // Filter out role items at this level and build the submenu.
    let filtered: Vec<_> = children
        .iter()
        .filter(|n| n.role == MenuRole::None)
        .collect();
    build_submenu_filtered(mtm, target, &filtered)
}

fn build_submenu_filtered<Message>(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    children: &[&MenuNode<Message>],
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);

    for node in children {
        // Skip items with special roles (they're already in the app menu).
        if node.role != MenuRole::None {
            continue;
        }

        match &node.kind {
            MenuKind::Separator => {
                let sep = NSMenuItem::separatorItem(mtm);
                submenu.addItem(&sep);
            }
            MenuKind::Submenu {
                label,
                enabled,
                children,
            } => {
                let item = NSMenuItem::new(mtm);
                item.setTitle(&NSString::from_str(&strip_mnemonic(label)));
                item.setEnabled(*enabled);

                let sub = build_submenu(mtm, target, children);
                item.setSubmenu(Some(&sub));

                submenu.addItem(&item);
            }
            MenuKind::Item {
                label,
                enabled,
                shortcut,
                on_activate: _,
            } => {
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    None,
                    shortcut.as_ref(),
                );
                submenu.addItem(&item);
            }
            MenuKind::CheckItem {
                label,
                enabled,
                checked,
                shortcut,
                on_activate: _,
            } => {
                let item = build_leaf_item(
                    mtm,
                    target,
                    &node.id,
                    label,
                    *enabled,
                    *checked,
                    shortcut.as_ref(),
                );
                submenu.addItem(&item);
            }
        }
    }

    submenu
}

fn build_leaf_item(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    id: &MenuId,
    label: &str,
    enabled: bool,
    checked: Option<bool>,
    shortcut: Option<&MenuShortcut>,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);

    item.setTitle(&NSString::from_str(&strip_mnemonic(label)));
    item.setEnabled(enabled);

    // Hook activation.
    // SAFETY: Objective-C object graph is kept alive by the installed NSMenu.
    #[allow(unsafe_code)]
    unsafe {
        item.setTarget(Some(target));
        item.setAction(Some(sel!(menuItemActivated:)));

        // Store MenuId in tag (u64 fits in isize on 64-bit platforms)
        let _: () = msg_send![&item, setTag: id.0 as isize];
    }

    // Checked state (NSControlStateValueOn=1, Off=0, Mixed=-1)
    // None -> no checkmark indicator (Off)
    // Some(false) -> unchecked (Off)
    // Some(true) -> checked (On)
    let state: i64 = match checked {
        Some(true) => 1,
        Some(false) | None => 0,
    };
    // SAFETY: Objective-C message send.
    #[allow(unsafe_code)]
    unsafe {
        let _: () = msg_send![&item, setState: state];
    }

    // Set keyboard shortcut if provided
    if let Some(shortcut) = shortcut {
        apply_shortcut(&item, shortcut);
    }

    item
}

/// Applies a keyboard shortcut to an NSMenuItem.
#[allow(unsafe_code)]
fn apply_shortcut(item: &NSMenuItem, shortcut: &MenuShortcut) {
    // Convert the key to a string for keyEquivalent
    let key_str = match &shortcut.key {
        Key::Character(c) => c.to_lowercase(),
        Key::Named(named) => {
            match named {
                Named::Enter => "\r".to_string(),
                Named::Tab => "\t".to_string(),
                Named::Escape => "\u{1b}".to_string(),
                Named::Backspace => "\u{08}".to_string(),
                Named::Delete => "\u{7f}".to_string(),
                Named::ArrowUp => "\u{f700}".to_string(),
                Named::ArrowDown => "\u{f701}".to_string(),
                Named::ArrowLeft => "\u{f702}".to_string(),
                Named::ArrowRight => "\u{f703}".to_string(),
                Named::Home => "\u{f729}".to_string(),
                Named::End => "\u{f72b}".to_string(),
                Named::PageUp => "\u{f72c}".to_string(),
                Named::PageDown => "\u{f72d}".to_string(),
                Named::F1 => "\u{f704}".to_string(),
                Named::F2 => "\u{f705}".to_string(),
                Named::F3 => "\u{f706}".to_string(),
                Named::F4 => "\u{f707}".to_string(),
                Named::F5 => "\u{f708}".to_string(),
                Named::F6 => "\u{f709}".to_string(),
                Named::F7 => "\u{f70a}".to_string(),
                Named::F8 => "\u{f70b}".to_string(),
                Named::F9 => "\u{f70c}".to_string(),
                Named::F10 => "\u{f70d}".to_string(),
                Named::F11 => "\u{f70e}".to_string(),
                Named::F12 => "\u{f70f}".to_string(),
                _ => return, // Unsupported key
            }
        }
        _ => return, // Unsupported key type
    };

    item.setKeyEquivalent(&NSString::from_str(&key_str));

    // Convert modifiers to macOS modifier mask
    // NSEventModifierFlagCommand = 1 << 20
    // NSEventModifierFlagShift = 1 << 17
    // NSEventModifierFlagOption = 1 << 19
    // NSEventModifierFlagControl = 1 << 18
    let mut modifier_mask: usize = 0;

    if shortcut.modifiers.contains(Modifiers::COMMAND) {
        modifier_mask |= 1 << 20; // NSEventModifierFlagCommand
    }
    if shortcut.modifiers.contains(Modifiers::SHIFT) {
        modifier_mask |= 1 << 17; // NSEventModifierFlagShift
    }
    if shortcut.modifiers.contains(Modifiers::ALT) {
        modifier_mask |= 1 << 19; // NSEventModifierFlagOption
    }
    if shortcut.modifiers.contains(Modifiers::CTRL) {
        modifier_mask |= 1 << 18; // NSEventModifierFlagControl
    }

    // SAFETY: Objective-C message send
    unsafe {
        let _: () = msg_send![item, setKeyEquivalentModifierMask: modifier_mask];
    }
}

fn menu_signature<Message>(menu: &AppMenu<Message>) -> u64 {
    use std::hash::Hasher;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for root in &menu.roots {
        hash_node(root, &mut hasher);
    }
    hasher.finish()
}

fn hash_node<Message>(node: &MenuNode<Message>, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;

    // Note: We intentionally do NOT hash node.id here because IDs are now
    // auto-generated and would change on every menu rebuild, causing flickering.
    node.role.hash(hasher);

    match &node.kind {
        MenuKind::Separator => {
            0u8.hash(hasher);
        }
        MenuKind::Item {
            label,
            enabled,
            shortcut,
            on_activate: _,
        } => {
            1u8.hash(hasher);
            label.hash(hasher);
            enabled.hash(hasher);
            shortcut.hash(hasher);
        }
        MenuKind::CheckItem {
            label,
            enabled,
            checked,
            shortcut,
            on_activate: _,
        } => {
            2u8.hash(hasher);
            label.hash(hasher);
            enabled.hash(hasher);
            checked.hash(hasher);
            shortcut.hash(hasher);
        }
        MenuKind::Submenu {
            label,
            enabled,
            children,
        } => {
            3u8.hash(hasher);
            label.hash(hasher);
            enabled.hash(hasher);
            for child in children {
                hash_node(child, hasher);
            }
        }
    }
}
