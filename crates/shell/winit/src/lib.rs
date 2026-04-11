//! A windowing shell for Iced, on top of [`winit`].
//!
//! ![The native path of the Iced ecosystem](https://github.com/iced-rs/iced/blob/0525d76ff94e828b7b21634fa94a747022001c83/docs/graphs/native.png?raw=true)
//!
//! `icy_ui_winit` offers some convenient abstractions on top of [`icy_ui_runtime`]
//! to quickstart development when using [`winit`].
//!
//! It exposes a renderer-agnostic [`Program`] trait that can be implemented
//! and then run with a simple call. The use of this trait is optional.
//!
//! Additionally, a [`conversion`] module is available for users that decide to
//! implement a custom event loop.
//!
//! [`icy_ui_runtime`]: https://github.com/iced-rs/iced/tree/master/runtime
//! [`winit`]: https://github.com/rust-windowing/winit
//! [`conversion`]: crate::conversion
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/iced-rs/iced/9ab6923e943f784985e9ef9ca28b10278297225d/docs/logo.svg"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
pub use icy_ui_debug as debug;
pub use icy_ui_program as program;
pub use program::core;
pub use program::graphics;
pub use program::runtime;
pub use runtime::futures;
pub use winit;

#[cfg(feature = "accessibility")]
pub mod accessibility;
pub mod clipboard;
pub mod conversion;
pub mod dnd;

mod error;
mod proxy;
mod window;

pub use clipboard::Clipboard;
pub use dnd::DndManager;
pub use error::Error;
pub use proxy::Proxy;

use crate::core::mouse;
use crate::core::renderer;
use crate::core::theme;
use crate::core::time::Instant;
use crate::core::widget::Operation;
use crate::core::widget::operation;
use crate::core::widget::operation::focusable::FocusLevel;
use crate::core::{Point, Size};
use crate::futures::futures::channel::mpsc;
use crate::futures::futures::channel::oneshot;
use crate::futures::futures::task;
use crate::futures::futures::{Future, StreamExt};
use crate::futures::subscription;
use crate::futures::{Executor, Runtime};
use crate::graphics::{Compositor, Shell, compositor};
use crate::runtime::image;
use crate::runtime::system;
use crate::runtime::user_interface::{self, UserInterface};
use crate::runtime::{Action, Task};

use program::Program;
use window::WindowManager;

use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::mem::ManuallyDrop;
use std::slice;
use std::sync::Arc;

#[cfg(target_os = "macos")]
use std::sync::mpsc as std_mpsc;

/// Runs a [`Program`] with the provided settings.
pub fn run<P>(program: P) -> Result<(), Error>
where
    P: Program<Theme = icy_ui_widget::Theme, Renderer = icy_ui_widget::Renderer> + 'static,
    P::Theme: theme::Base,
    P::Message: Clone,
{
    use winit::event_loop::EventLoop;

    let boot_span = debug::boot();
    let settings = program.settings();
    let window_settings = program.window();

    // Install URL handler on macOS before event loop starts
    #[cfg(target_os = "macos")]
    let url_receiver = icy_ui_macos::UrlHandler::install().into_receiver();

    let event_loop = EventLoop::with_user_event()
        .build()
        .expect("Create event loop");

    let graphics_settings = settings.clone().into();
    let display_handle = event_loop.owned_display_handle();

    let (proxy, worker) = Proxy::new(event_loop.create_proxy());

    #[cfg(feature = "debug")]
    {
        let proxy = proxy.clone();

        debug::on_hotpatch(move || {
            proxy.send_action(Action::Reload);
        });
    }

    let mut runtime = {
        let executor = P::Executor::new().map_err(Error::ExecutorCreationFailed)?;
        executor.spawn(worker);

        Runtime::new(executor, proxy.clone())
    };

    let (program, task) = runtime.enter(|| program::Instance::new(program));
    let is_daemon = window_settings.is_none();

    let task = if let Some(window_settings) = window_settings {
        let mut task = Some(task);

        let (_id, open) = runtime::window::open(window_settings);

        open.then(move |_| task.take().unwrap_or_else(Task::none))
    } else {
        task
    };

    if let Some(stream) = runtime::task::into_stream(task) {
        runtime.run(stream);
    }

    runtime.track(subscription::into_recipes(
        runtime.enter(|| program.subscription().map(Action::Output)),
    ));

    let (event_sender, event_receiver) = mpsc::unbounded();
    let (control_sender, control_receiver) = mpsc::unbounded();
    let (system_theme_sender, system_theme_receiver) = oneshot::channel();

    let instance = Box::pin(run_instance::<P>(
        program,
        runtime,
        proxy.clone(),
        event_receiver,
        control_sender,
        display_handle,
        is_daemon,
        graphics_settings,
        settings.fonts,
        settings.focus_level,
        system_theme_receiver,
        #[cfg(target_os = "macos")]
        url_receiver,
    ));

    let context = task::Context::from_waker(task::noop_waker_ref());

    struct Runner<Message: 'static, F> {
        instance: std::pin::Pin<Box<F>>,
        context: task::Context<'static>,
        id: Option<String>,
        sender: mpsc::UnboundedSender<Event<Action<Message>>>,
        receiver: mpsc::UnboundedReceiver<Control>,
        error: Option<Error>,
        system_theme: Option<oneshot::Sender<theme::Mode>>,

        #[cfg(target_os = "macos")]
        mac_menu: Option<icy_ui_macos::MacMenu>,

        #[cfg(target_arch = "wasm32")]
        canvas: Option<web_sys::HtmlCanvasElement>,
    }

    let runner = Runner {
        instance,
        context,
        id: settings.id,
        sender: event_sender,
        receiver: control_receiver,
        error: None,
        system_theme: Some(system_theme_sender),

        #[cfg(target_os = "macos")]
        mac_menu: icy_ui_macos::MacMenu::new().ok(),

        #[cfg(target_arch = "wasm32")]
        canvas: None,
    };

    boot_span.finish();

    impl<Message, F> winit::application::ApplicationHandler<Action<Message>> for Runner<Message, F>
    where
        F: Future<Output = ()>,
    {
        fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            if let Some(sender) = self.system_theme.take() {
                let _ = sender.send(
                    event_loop
                        .system_theme()
                        .map(conversion::theme_mode)
                        .unwrap_or_default(),
                );
            }
        }

        fn new_events(
            &mut self,
            event_loop: &winit::event_loop::ActiveEventLoop,
            cause: winit::event::StartCause,
        ) {
            self.process_event(
                event_loop,
                Event::EventLoopAwakened(winit::event::Event::NewEvents(cause)),
            );
        }

        fn window_event(
            &mut self,
            event_loop: &winit::event_loop::ActiveEventLoop,
            window_id: winit::window::WindowId,
            event: winit::event::WindowEvent,
        ) {
            #[cfg(target_os = "windows")]
            let is_move_or_resize = matches!(
                event,
                winit::event::WindowEvent::Resized(_) | winit::event::WindowEvent::Moved(_)
            );

            self.process_event(
                event_loop,
                Event::EventLoopAwakened(winit::event::Event::WindowEvent { window_id, event }),
            );

            // TODO: Remove when unnecessary
            // On Windows, we emulate an `AboutToWait` event after every `Resized` event
            // since the event loop does not resume during resize interaction.
            // More details: https://github.com/rust-windowing/winit/issues/3272
            #[cfg(target_os = "windows")]
            {
                if is_move_or_resize {
                    self.process_event(
                        event_loop,
                        Event::EventLoopAwakened(winit::event::Event::AboutToWait),
                    );
                }
            }
        }

        fn user_event(
            &mut self,
            event_loop: &winit::event_loop::ActiveEventLoop,
            action: Action<Message>,
        ) {
            self.process_event(
                event_loop,
                Event::EventLoopAwakened(winit::event::Event::UserEvent(action)),
            );
        }

        fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            #[cfg(target_os = "macos")]
            {
                if let Some(menu) = self.mac_menu.as_mut() {
                    while let Some(id) = menu.try_recv() {
                        if self.sender.start_send(Event::MenuActivated(id)).is_err() {
                            event_loop.exit();
                            return;
                        }
                    }
                }
            }

            self.process_event(
                event_loop,
                Event::EventLoopAwakened(winit::event::Event::AboutToWait),
            );
        }
    }

    impl<Message, F> Runner<Message, F>
    where
        F: Future<Output = ()>,
    {
        fn process_event(
            &mut self,
            event_loop: &winit::event_loop::ActiveEventLoop,
            event: Event<Action<Message>>,
        ) {
            if event_loop.exiting() {
                return;
            }

            if self.sender.start_send(event).is_err() {
                // Channel disconnected, exit gracefully
                event_loop.exit();
                return;
            }

            loop {
                let poll = self.instance.as_mut().poll(&mut self.context);

                match poll {
                    task::Poll::Pending => match self.receiver.try_next() {
                        Ok(Some(control)) => match control {
                            Control::ChangeFlow(flow) => {
                                use winit::event_loop::ControlFlow;

                                match (event_loop.control_flow(), flow) {
                                    (
                                        ControlFlow::WaitUntil(current),
                                        ControlFlow::WaitUntil(new),
                                    ) if current < new => {}
                                    (ControlFlow::WaitUntil(target), ControlFlow::Wait)
                                        if target > Instant::now() => {}
                                    _ => {
                                        event_loop.set_control_flow(flow);
                                    }
                                }
                            }
                            Control::CreateWindow {
                                id,
                                settings,
                                title,
                                scale_factor,
                                monitor,
                                on_open,
                            } => {
                                let exit_on_close_request = settings.exit_on_close_request;

                                let visible = settings.visible;

                                #[cfg(target_arch = "wasm32")]
                                let target = settings.platform_specific.target.clone();

                                let window_attributes = conversion::window_attributes(
                                    settings,
                                    &title,
                                    scale_factor,
                                    monitor.or(event_loop.primary_monitor()),
                                    self.id.clone(),
                                )
                                .with_visible(false);

                                #[cfg(target_arch = "wasm32")]
                                let window_attributes = {
                                    use winit::platform::web::WindowAttributesExtWebSys;
                                    window_attributes.with_canvas(self.canvas.take())
                                };

                                log::info!(
                                    "Window attributes for id `{id:#?}`: {window_attributes:#?}"
                                );

                                // On macOS, the `position` in `WindowAttributes` represents the "inner"
                                // position of the window; while on other platforms it's the "outer" position.
                                // We fix the inconsistency on macOS by positioning the window after creation.
                                #[cfg(target_os = "macos")]
                                let mut window_attributes = window_attributes;

                                #[cfg(target_os = "macos")]
                                let position = window_attributes.position.take();

                                let window = event_loop
                                    .create_window(window_attributes)
                                    .expect("Create window");

                                #[cfg(feature = "accessibility")]
                                let accessibility =
                                    Some(crate::accessibility::AccessibilityAdapter::new(
                                        event_loop, &window,
                                    ));

                                #[cfg(not(feature = "accessibility"))]
                                let _accessibility = ();

                                #[cfg(target_os = "macos")]
                                if let Some(position) = position {
                                    window.set_outer_position(position);
                                }

                                #[cfg(target_arch = "wasm32")]
                                {
                                    use winit::platform::web::WindowExtWebSys;

                                    let canvas = window.canvas().expect("Get window canvas");

                                    let _ = canvas.set_attribute(
                                        "style",
                                        "display: block; width: 100%; height: 100%",
                                    );

                                    let window = web_sys::window().unwrap();
                                    let document = window.document().unwrap();
                                    let body = document.body().unwrap();

                                    let target = target.and_then(|target| {
                                        body.query_selector(&format!("#{target}"))
                                            .ok()
                                            .unwrap_or(None)
                                    });

                                    match target {
                                        Some(node) => {
                                            let _ = node.replace_with_with_node_1(&canvas).expect(
                                                &format!("Could not replace #{}", node.id()),
                                            );
                                        }
                                        None => {
                                            let _ = body
                                                .append_child(&canvas)
                                                .expect("Append canvas to HTML body");
                                        }
                                    };
                                }

                                self.process_event(
                                    event_loop,
                                    Event::WindowCreated {
                                        id,
                                        window: Arc::new(window),
                                        exit_on_close_request,
                                        make_visible: visible,
                                        #[cfg(feature = "accessibility")]
                                        accessibility,
                                        on_open,
                                    },
                                );
                            }
                            Control::Exit => {
                                self.process_event(event_loop, Event::Exit);
                                event_loop.exit();
                                break;
                            }
                            Control::Crash(error) => {
                                self.error = Some(error);
                                event_loop.exit();
                            }
                            Control::SetAutomaticWindowTabbing(_enabled) => {
                                #[cfg(target_os = "macos")]
                                {
                                    use winit::platform::macos::ActiveEventLoopExtMacOS;
                                    event_loop.set_allows_automatic_window_tabbing(_enabled);
                                }
                            }

                            #[cfg(target_os = "macos")]
                            Control::SetApplicationMenu(menu) => {
                                if let (Some(mac_menu), Some(menu)) = (self.mac_menu.as_mut(), menu)
                                {
                                    if let Err(error) = mac_menu.sync(&menu) {
                                        log::warn!(
                                            "Failed to sync macOS application menu: {error}"
                                        );
                                    }
                                }
                            }
                        },
                        _ => {
                            break;
                        }
                    },
                    task::Poll::Ready(_) => {
                        event_loop.exit();
                        break;
                    }
                };
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut runner = runner;
        let _ = event_loop.run_app(&mut runner);

        runner.error.map(Err).unwrap_or(Ok(()))
    }

    #[cfg(target_arch = "wasm32")]
    {
        use winit::platform::web::EventLoopExtWebSys;
        let _ = event_loop.spawn_app(runner);

        Ok(())
    }
}

#[derive(Debug)]
enum Event<Message: 'static> {
    WindowCreated {
        id: window::Id,
        window: Arc<winit::window::Window>,
        exit_on_close_request: bool,
        make_visible: bool,
        #[cfg(feature = "accessibility")]
        accessibility: Option<crate::accessibility::AccessibilityAdapter>,
        on_open: oneshot::Sender<window::Id>,
    },
    EventLoopAwakened(winit::event::Event<Message>),

    #[cfg(target_os = "macos")]
    MenuActivated(core::menu::MenuId),

    Exit,
}

#[derive(Debug)]
enum Control {
    ChangeFlow(winit::event_loop::ControlFlow),
    Exit,
    Crash(Error),
    CreateWindow {
        id: window::Id,
        settings: window::Settings,
        title: String,
        monitor: Option<winit::monitor::MonitorHandle>,
        on_open: oneshot::Sender<window::Id>,
        scale_factor: f32,
    },
    SetAutomaticWindowTabbing(bool),

    #[cfg(target_os = "macos")]
    SetApplicationMenu(Option<core::menu::AppMenu<core::menu::MenuId>>),
}

#[cfg(feature = "accessibility")]
struct AccessibilityState {
    /// The accesskit adapter for this window.
    adapter: crate::accessibility::AccessibilityAdapter,
    /// Core accessibility state (mode, focus, announcements).
    state: crate::core::accessibility::AccessibilityState,
    /// The last focused node id, used to detect focus changes and announce them.
    last_focused: Option<crate::core::accessibility::NodeId>,
}

async fn run_instance<P>(
    mut program: program::Instance<P>,
    mut runtime: Runtime<P::Executor, Proxy<P::Message>, Action<P::Message>>,
    mut proxy: Proxy<P::Message>,
    mut event_receiver: mpsc::UnboundedReceiver<Event<Action<P::Message>>>,
    mut control_sender: mpsc::UnboundedSender<Control>,
    display_handle: winit::event_loop::OwnedDisplayHandle,
    is_daemon: bool,
    graphics_settings: graphics::Settings,
    default_fonts: Vec<Cow<'static, [u8]>>,
    focus_level: FocusLevel,
    mut _system_theme: oneshot::Receiver<theme::Mode>,
    #[cfg(target_os = "macos")] url_receiver: std_mpsc::Receiver<String>,
) where
    P: Program<Theme = icy_ui_widget::Theme, Renderer = icy_ui_widget::Renderer> + 'static,
    P::Theme: theme::Base,
    P::Message: Clone,
{
    use winit::event;
    use winit::event_loop::ControlFlow;

    let mut window_manager = WindowManager::new();
    let mut is_window_opening = !is_daemon;

    let mut compositor = None;
    let mut events = Vec::new();
    let mut messages = Vec::new();
    let mut actions = 0;

    let mut ui_caches = FxHashMap::default();
    let mut user_interfaces = ManuallyDrop::new(FxHashMap::default());
    let mut clipboard = Clipboard::unconnected();
    let mut dnd_manager = DndManager::unconnected();

    #[cfg(target_os = "macos")]
    let mut mac_menu_actions: FxHashMap<core::menu::MenuId, P::Message> = FxHashMap::default();

    #[cfg(target_os = "macos")]
    let mut mac_menu_signature: u64 = 0;

    #[cfg(target_os = "macos")]
    let mac_context_menu = icy_ui_macos::MacContextMenu::new().ok();

    #[cfg(feature = "accessibility")]
    let mut accessibility: FxHashMap<window::Id, AccessibilityState> = FxHashMap::default();

    #[cfg(all(feature = "linux-theme-detection", target_os = "linux"))]
    let mut system_theme = {
        let to_mode = |color_scheme| match color_scheme {
            mundy::ColorScheme::NoPreference => theme::Mode::None,
            mundy::ColorScheme::Light => theme::Mode::Light,
            mundy::ColorScheme::Dark => theme::Mode::Dark,
        };

        runtime.run(
            mundy::Preferences::stream(mundy::Interest::ColorScheme)
                .map(move |preferences| {
                    Action::System(system::Action::NotifyTheme(to_mode(
                        preferences.color_scheme,
                    )))
                })
                .boxed(),
        );

        runtime
            .enter(|| {
                mundy::Preferences::once_blocking(
                    mundy::Interest::ColorScheme,
                    core::time::Duration::from_millis(200),
                )
            })
            .map(|preferences| to_mode(preferences.color_scheme))
            .unwrap_or_default()
    };

    #[cfg(not(all(feature = "linux-theme-detection", target_os = "linux")))]
    let mut system_theme = _system_theme.try_recv().ok().flatten().unwrap_or_default();

    log::info!("System theme: {system_theme:?}");

    'next_event: loop {
        // Empty the queue if possible
        let event = if let Ok(event) = event_receiver.try_next() {
            event
        } else {
            event_receiver.next().await
        };

        let Some(event) = event else {
            break;
        };

        match event {
            Event::WindowCreated {
                id,
                window,
                exit_on_close_request,
                make_visible,
                #[cfg(feature = "accessibility")]
                    accessibility: a11y_adapter,
                on_open,
            } => {
                if compositor.is_none() {
                    let (compositor_sender, compositor_receiver) = oneshot::channel();

                    let create_compositor = {
                        let window = window.clone();
                        let display_handle = display_handle.clone();
                        let proxy = proxy.clone();
                        let default_fonts = default_fonts.clone();

                        async move {
                            let shell = Shell::new(proxy.clone());

                            let mut compositor =
                                <P::Renderer as compositor::Default>::Compositor::new(
                                    graphics_settings,
                                    display_handle,
                                    window,
                                    shell,
                                )
                                .await;

                            if let Ok(compositor) = &mut compositor {
                                for font in default_fonts {
                                    compositor.load_font(font.clone());
                                }
                            }

                            compositor_sender
                                .send(compositor)
                                .ok()
                                .expect("Send compositor");

                            // HACK! Send a proxy event on completion to trigger
                            // a runtime re-poll
                            // TODO: Send compositor through proxy (?)
                            {
                                let (sender, _receiver) = oneshot::channel();

                                proxy.send_action(Action::Window(
                                    runtime::window::Action::GetLatest(sender),
                                ));
                            }
                        }
                    };

                    #[cfg(target_arch = "wasm32")]
                    wasm_bindgen_futures::spawn_local(create_compositor);

                    #[cfg(not(target_arch = "wasm32"))]
                    runtime.block_on(create_compositor);

                    match compositor_receiver.await.expect("Wait for compositor") {
                        Ok(new_compositor) => {
                            compositor = Some(new_compositor);
                        }
                        Err(error) => {
                            let _ = control_sender.start_send(Control::Crash(error.into()));
                            continue;
                        }
                    }
                }

                let window_theme = window
                    .theme()
                    .map(conversion::theme_mode)
                    .unwrap_or_default();

                if system_theme != window_theme {
                    system_theme = window_theme;

                    runtime.broadcast(subscription::Event::SystemThemeChanged(window_theme));
                }

                let is_first = window_manager.is_empty();
                let window = window_manager.insert(
                    id,
                    window,
                    &program,
                    compositor.as_mut().expect("Compositor must be initialized"),
                    exit_on_close_request,
                    system_theme,
                );

                window
                    .raw
                    .set_theme(conversion::window_theme(window.state.theme_mode()));

                debug::theme_changed(|| {
                    if is_first {
                        theme::Base::palette(window.state.theme())
                    } else {
                        None
                    }
                });

                let logical_size = window.state.logical_size();

                #[cfg(feature = "hinting")]
                {
                    use crate::core::Renderer as _;
                    window.renderer.hint(window.state.scale_factor());
                }

                let menu_context = core::menu::MenuContext {
                    current_window: Some(id),
                    windows: vec![core::menu::WindowInfo {
                        id,
                        title: window.state.title().to_owned(),
                        focused: window.state.focused(),
                        minimized: window.raw.is_minimized().unwrap_or(false),
                    }],
                };

                let _ = user_interfaces.insert(
                    id,
                    build_user_interface(
                        &program,
                        user_interface::Cache::default(),
                        &mut window.renderer,
                        logical_size,
                        &menu_context,
                        id,
                    ),
                );
                let _ = ui_caches.insert(id, user_interface::Cache::default());

                #[cfg(feature = "accessibility")]
                if let Some(adapter) = a11y_adapter {
                    let _ = accessibility.insert(
                        id,
                        AccessibilityState {
                            adapter,
                            state: crate::core::accessibility::AccessibilityState::new(),
                            last_focused: None,
                        },
                    );
                }

                if make_visible {
                    window.raw.set_visible(true);
                }

                events.push((
                    id,
                    core::Event::Window(window::Event::Opened {
                        position: window.position(),
                        size: window.logical_size(),
                    }),
                ));

                if clipboard.window_id().is_none() {
                    clipboard = Clipboard::connect(window.raw.clone());
                }

                // Connect DnD manager if not yet initialized
                if !dnd_manager.is_available() {
                    let proxy_for_dnd = proxy.clone();
                    let wakeup = std::sync::Arc::new(move || {
                        proxy_for_dnd.wake_up();
                    });
                    dnd_manager = DndManager::connect(window.raw.clone(), wakeup);
                }

                let _ = on_open.send(id);
                is_window_opening = false;
            }

            #[cfg(target_os = "macos")]
            Event::MenuActivated(id) => {
                if let Some(message) = mac_menu_actions.remove(&id) {
                    messages.push(message);
                }
            }

            Event::EventLoopAwakened(event) => {
                match event {
                    event::Event::NewEvents(start_cause) => {
                        // Poll for DnD events from smithay-clipboard on every event loop iteration
                        for dnd_event in dnd_manager.poll_events() {
                            // DnD events are window-agnostic, use the first window
                            if let Some((id, _)) = window_manager.iter_mut().next() {
                                events.push((id, dnd_event));
                            }
                        }

                        match start_cause {
                            event::StartCause::Init => {
                                for (_id, window) in window_manager.iter_mut() {
                                    window.raw.request_redraw();
                                }
                            }
                            event::StartCause::ResumeTimeReached { .. } => {
                                let now = Instant::now();

                                for (_id, window) in window_manager.iter_mut() {
                                    if let Some(redraw_at) = window.redraw_at
                                        && redraw_at <= now
                                    {
                                        window.raw.request_redraw();
                                        window.redraw_at = None;
                                    }
                                }

                                if let Some(redraw_at) = window_manager.redraw_at() {
                                    let _ = control_sender.start_send(Control::ChangeFlow(
                                        ControlFlow::WaitUntil(redraw_at),
                                    ));
                                } else {
                                    let _ = control_sender
                                        .start_send(Control::ChangeFlow(ControlFlow::Wait));
                                }
                            }
                            _ => {}
                        }
                    }
                    event::Event::UserEvent(action) => {
                        run_action(
                            action,
                            &program,
                            &mut runtime,
                            &mut compositor,
                            &mut events,
                            &mut messages,
                            &mut clipboard,
                            &mut dnd_manager,
                            &mut control_sender,
                            &mut user_interfaces,
                            &mut window_manager,
                            &mut ui_caches,
                            &mut is_window_opening,
                            #[cfg(feature = "accessibility")]
                            &mut accessibility,
                            &mut system_theme,
                        );
                        actions += 1;
                    }
                    event::Event::WindowEvent {
                        window_id: id,
                        event: event::WindowEvent::RedrawRequested,
                        ..
                    } => {
                        let Some(mut current_compositor) = compositor.as_mut() else {
                            continue;
                        };

                        let Some((id, mut window)) = window_manager.get_mut_alias(id) else {
                            continue;
                        };

                        let physical_size = window.state.physical_size();
                        let mut logical_size = window.state.logical_size();

                        if physical_size.width == 0 || physical_size.height == 0 {
                            continue;
                        }

                        // Window was resized between redraws
                        if window.surface_version != window.state.surface_version() {
                            #[cfg(feature = "hinting")]
                            {
                                use crate::core::Renderer as _;
                                window.renderer.hint(window.state.scale_factor());
                            }

                            let ui = user_interfaces.remove(&id).expect("Remove user interface");

                            let layout_span = debug::layout(id);
                            let _ = user_interfaces
                                .insert(id, ui.relayout(logical_size, &mut window.renderer));
                            layout_span.finish();

                            current_compositor.configure_surface(
                                &mut window.surface,
                                physical_size.width,
                                physical_size.height,
                            );

                            window.surface_version = window.state.surface_version();
                        }

                        let redraw_event =
                            core::Event::Window(window::Event::RedrawRequested(Instant::now()));

                        let cursor = window.state.cursor();

                        let mut interface =
                            user_interfaces.get_mut(&id).expect("Get user interface");

                        let interact_span = debug::interact(id);
                        let mut redraw_count = 0;

                        let state = loop {
                            let message_count = messages.len();
                            let (state, _) = interface.update(
                                slice::from_ref(&redraw_event),
                                cursor,
                                &mut window.renderer,
                                &mut clipboard,
                                &mut messages,
                            );

                            if message_count == messages.len() && !state.has_layout_changed() {
                                break state;
                            }

                            if redraw_count >= 2 {
                                log::warn!(
                                    "More than 3 consecutive RedrawRequested events \
                                    produced layout invalidation"
                                );

                                break state;
                            }

                            redraw_count += 1;

                            if !messages.is_empty() {
                                let caches: FxHashMap<_, _> =
                                    ManuallyDrop::into_inner(user_interfaces)
                                        .into_iter()
                                        .map(|(id, interface)| (id, interface.into_cache()))
                                        .collect();

                                let actions = update(&mut program, &mut runtime, &mut messages);

                                user_interfaces = ManuallyDrop::new(build_user_interfaces(
                                    &program,
                                    &mut window_manager,
                                    caches,
                                ));

                                for action in actions {
                                    // Defer all window actions to avoid compositor
                                    // race conditions while redrawing
                                    if let Action::Window(_) = action {
                                        proxy.send_action(action);
                                        continue;
                                    }

                                    run_action(
                                        action,
                                        &program,
                                        &mut runtime,
                                        &mut compositor,
                                        &mut events,
                                        &mut messages,
                                        &mut clipboard,
                                        &mut dnd_manager,
                                        &mut control_sender,
                                        &mut user_interfaces,
                                        &mut window_manager,
                                        &mut ui_caches,
                                        &mut is_window_opening,
                                        #[cfg(feature = "accessibility")]
                                        &mut accessibility,
                                        &mut system_theme,
                                    );
                                }

                                for (window_id, window) in window_manager.iter_mut() {
                                    // We are already redrawing this window
                                    if window_id == id {
                                        continue;
                                    }

                                    window.raw.request_redraw();
                                }

                                let Some(next_compositor) = compositor.as_mut() else {
                                    continue 'next_event;
                                };

                                current_compositor = next_compositor;
                                window = window_manager.get_mut(id).unwrap();

                                // Window scale factor changed during a redraw request
                                if logical_size != window.state.logical_size() {
                                    logical_size = window.state.logical_size();

                                    log::debug!(
                                        "Window scale factor changed during a redraw request"
                                    );

                                    let ui =
                                        user_interfaces.remove(&id).expect("Remove user interface");

                                    let layout_span = debug::layout(id);
                                    let _ = user_interfaces.insert(
                                        id,
                                        ui.relayout(logical_size, &mut window.renderer),
                                    );
                                    layout_span.finish();
                                }

                                interface = user_interfaces.get_mut(&id).unwrap();
                            }
                        };
                        interact_span.finish();

                        #[cfg(feature = "accessibility")]
                        if let user_interface::State::Updated {
                            a11y_focus_request: Some(target),
                            ..
                        } = state
                            && let Some(acc) = accessibility.get_mut(&id)
                        {
                            // Move accessibility focus (VoiceOver cursor) programmatically.
                            acc.state.set_a11y_focus(target);
                        }

                        let draw_span = debug::draw(id);
                        interface.draw(
                            &mut window.renderer,
                            window.state.theme(),
                            &renderer::Style {
                                text_color: window.state.text_color(),
                            },
                            cursor,
                        );
                        draw_span.finish();

                        #[cfg(feature = "accessibility")]
                        if let Some(state) = accessibility.get_mut(&id)
                            && state.state.is_active()
                            && state.adapter.is_enabled()
                        {
                            update_accessibility_tree(id, window, interface, state);
                        }

                        if let user_interface::State::Updated {
                            redraw_request,
                            input_method,
                            mouse_interaction,
                            #[cfg(target_os = "macos")]
                            context_menu_request,
                            ..
                        } = state
                        {
                            window.request_redraw(redraw_request);
                            window.request_input_method(input_method);
                            window.update_mouse(mouse_interaction);

                            // Handle native context menu request from widget
                            #[cfg(target_os = "macos")]
                            if let Some(ctx_req) = context_menu_request {
                                if let Some(ctx_menu) = &mac_context_menu {
                                    use winit::raw_window_handle::{
                                        HasWindowHandle, RawWindowHandle,
                                    };
                                    if let Ok(handle) = window.raw.window_handle() {
                                        if let RawWindowHandle::AppKit(appkit_handle) =
                                            handle.as_raw()
                                        {
                                            let ns_view_ptr = appkit_handle.ns_view.as_ptr()
                                                as *mut std::ffi::c_void;

                                            // Position is passed in iced coordinates (top-left origin)
                                            // The macOS menu function handles the conversion internally
                                            #[allow(unsafe_code)]
                                            if let Some(selected_id) = unsafe {
                                                ctx_menu.show_items_and_wait(
                                                    &ctx_req.items,
                                                    ns_view_ptr,
                                                    f64::from(ctx_req.position.x),
                                                    f64::from(ctx_req.position.y),
                                                )
                                            } {
                                                events.push((
                                                    id,
                                                    core::Event::ContextMenuItemSelected(
                                                        selected_id,
                                                    ),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        runtime.broadcast(subscription::Event::Interaction {
                            window: id,
                            event: redraw_event,
                            status: core::event::Status::Ignored,
                        });

                        window.draw_preedit();

                        let present_span = debug::present(id);
                        match current_compositor.present(
                            &mut window.renderer,
                            &mut window.surface,
                            window.state.viewport(),
                            window.state.background_color(),
                            || window.raw.pre_present_notify(),
                        ) {
                            Ok(()) => {
                                present_span.finish();
                            }
                            Err(error) => match error {
                                compositor::SurfaceError::OutOfMemory => {
                                    // This is an unrecoverable error.
                                    panic!("{error:?}");
                                }
                                compositor::SurfaceError::Outdated
                                | compositor::SurfaceError::Lost => {
                                    present_span.finish();

                                    // Reconfigure surface and try redrawing
                                    let physical_size = window.state.physical_size();

                                    if error == compositor::SurfaceError::Lost {
                                        window.surface = current_compositor.create_surface(
                                            window.raw.clone(),
                                            physical_size.width,
                                            physical_size.height,
                                        );
                                    } else {
                                        current_compositor.configure_surface(
                                            &mut window.surface,
                                            physical_size.width,
                                            physical_size.height,
                                        );
                                    }

                                    window.raw.request_redraw();
                                }
                                _ => {
                                    present_span.finish();

                                    log::error!(
                                        "Error {error:?} when \
                                        presenting surface."
                                    );

                                    // Try rendering all windows again next frame.
                                    for (_id, window) in window_manager.iter_mut() {
                                        window.raw.request_redraw();
                                    }
                                }
                            },
                        }
                    }
                    event::Event::WindowEvent {
                        event: window_event,
                        window_id,
                    } => {
                        if !is_daemon
                            && matches!(window_event, winit::event::WindowEvent::Destroyed)
                            && !is_window_opening
                            && window_manager.is_empty()
                        {
                            control_sender
                                .start_send(Control::Exit)
                                .expect("Send control action");

                            continue;
                        }

                        let Some((id, window)) = window_manager.get_mut_alias(window_id) else {
                            continue;
                        };

                        #[cfg(feature = "accessibility")]
                        if let Some(state) = accessibility.get_mut(&id) {
                            if crate::accessibility::trace_enabled() {
                                eprintln!(
                                    "[a11y] process_event window={id:?} winit_event={:?}",
                                    window_event
                                );
                            }

                            state
                                .adapter
                                .process_event(window.raw.as_ref(), &window_event);
                        }

                        match window_event {
                            winit::event::WindowEvent::Resized(_) => {
                                window.raw.request_redraw();
                            }
                            winit::event::WindowEvent::ThemeChanged(theme) => {
                                let mode = conversion::theme_mode(theme);

                                if mode != system_theme {
                                    system_theme = mode;

                                    runtime
                                        .broadcast(subscription::Event::SystemThemeChanged(mode));
                                }
                            }
                            _ => {}
                        }

                        if matches!(window_event, winit::event::WindowEvent::CloseRequested)
                            && window.exit_on_close_request
                        {
                            run_action(
                                Action::Window(runtime::window::Action::Close(id)),
                                &program,
                                &mut runtime,
                                &mut compositor,
                                &mut events,
                                &mut messages,
                                &mut clipboard,
                                &mut dnd_manager,
                                &mut control_sender,
                                &mut user_interfaces,
                                &mut window_manager,
                                &mut ui_caches,
                                &mut is_window_opening,
                                #[cfg(feature = "accessibility")]
                                &mut accessibility,
                                &mut system_theme,
                            );
                        } else {
                            window.state.update(&program, &window.raw, &window_event);

                            // Keep DnD modifier state in sync.
                            // Wayland DnD motion events do not include modifiers, so the DnD
                            // manager attaches this state when emitting `DragMoved`.
                            dnd_manager
                                .set_modifiers(conversion::modifiers(window.state.modifiers()));

                            if let Some(event) = conversion::window_event(
                                window_event,
                                window.state.scale_factor(),
                                window.state.modifiers(),
                            ) {
                                events.push((id, event));
                            }
                        }
                    }
                    event::Event::AboutToWait => {
                        if actions > 0 {
                            proxy.free_slots(actions);
                            actions = 0;
                        }

                        // Poll for URL events on macOS
                        #[cfg(target_os = "macos")]
                        while let Ok(url) = url_receiver.try_recv() {
                            runtime.broadcast(subscription::Event::PlatformSpecific(
                                subscription::PlatformSpecific::MacOS(
                                    subscription::MacOS::ReceivedUrl(url),
                                ),
                            ));
                        }

                        #[cfg(target_os = "macos")]
                        {
                            let mut windows = Vec::new();

                            for (id, window) in window_manager.iter_mut() {
                                windows.push(core::menu::WindowInfo {
                                    id,
                                    title: window.state.title().to_owned(),
                                    focused: window.state.focused(),
                                    minimized: window.raw.is_minimized().unwrap_or(false),
                                });
                            }

                            let focused_window = windows.iter().find(|w| w.focused).map(|w| w.id);
                            let menu_context = core::menu::MenuContext {
                                current_window: focused_window,
                                windows,
                            };

                            if let Some(menu) = program.application_menu(&menu_context) {
                                let (menu_for_platform, actions, signature) =
                                    macos_menu::menu_for_platform(menu);

                                mac_menu_actions = actions;

                                if signature != mac_menu_signature {
                                    mac_menu_signature = signature;
                                    let _ = control_sender.start_send(Control::SetApplicationMenu(
                                        Some(menu_for_platform),
                                    ));
                                }
                            } else {
                                mac_menu_actions.clear();

                                if mac_menu_signature != 0 {
                                    mac_menu_signature = 0;
                                    let _ = control_sender
                                        .start_send(Control::SetApplicationMenu(None));
                                }
                            }
                        }

                        #[cfg(feature = "accessibility")]
                        {
                            use crate::accessibility::ProcessedEvent;
                            use accesskit::Action;

                            for (id, state) in accessibility.iter_mut() {
                                for event in state.adapter.drain_events() {
                                    match event {
                                        ProcessedEvent::Activated => {
                                            // Screen reader connected - enter accessibility mode
                                            state.state.activate();
                                            crate::core::accessibility::set_accessibility_active(
                                                true,
                                            );
                                            log::info!(
                                                "Accessibility mode activated for window {:?}",
                                                id
                                            );
                                            if let Some(window) = window_manager.get_mut(*id) {
                                                window.raw.request_redraw();

                                                // Publish an initial tree immediately so the
                                                // screen reader has content even if the app is idle.
                                                if let Some(ui) = user_interfaces.get_mut(id) {
                                                    update_accessibility_tree(
                                                        *id, window, ui, state,
                                                    );
                                                }
                                            }
                                        }
                                        ProcessedEvent::ActionRequested(ref acc_event) => {
                                            if crate::accessibility::trace_enabled() {
                                                eprintln!(
                                                    "[a11y] ActionRequested: {:?} on {:?}",
                                                    acc_event.action, acc_event.target
                                                );
                                            }

                                            // For Click actions, set focus and pass the event
                                            // The widget will handle the click via Event::Accessibility
                                            if acc_event.action == Action::Click
                                                || acc_event.action == Action::Focus
                                            {
                                                // Set the a11y focus
                                                state.state.set_a11y_focus(acc_event.target);

                                                // Set widget focus to the target
                                                if let Some(ui) = user_interfaces.get_mut(id) {
                                                    if let Some(window) =
                                                        window_manager.get_mut(*id)
                                                    {
                                                        let mut focus_op: Box<dyn Operation<()>> =
                                                            Box::new(
                                                                operation::accessibility::focus_widget_by_node_id(
                                                                    acc_event.target,
                                                                ),
                                                            );
                                                        ui.operate(
                                                            &window.renderer,
                                                            focus_op.as_mut(),
                                                        );
                                                    }
                                                }
                                            }

                                            // Pass the accessibility event to widgets
                                            // Widgets handle Click via Event::Accessibility
                                            events.push((
                                                *id,
                                                core::Event::Accessibility(acc_event.clone()),
                                            ));
                                        }
                                        ProcessedEvent::Deactivated => {
                                            // Screen reader disconnected - exit accessibility mode
                                            if crate::accessibility::trace_enabled() {
                                                eprintln!(
                                                    "[a11y] Deactivated event received for window {:?}",
                                                    id
                                                );
                                            }
                                            state.state.deactivate();
                                            crate::core::accessibility::set_accessibility_active(
                                                false,
                                            );
                                            log::info!(
                                                "Accessibility mode deactivated for window {:?}",
                                                id
                                            );
                                        }
                                        ProcessedEvent::InitialTreeRequested => {
                                            // Not used by the direct-handler adapter.
                                        }
                                    }
                                }
                            }
                        }

                        if events.is_empty() && messages.is_empty() && window_manager.is_idle() {
                            continue;
                        }

                        let mut uis_stale = false;

                        for (id, window) in window_manager.iter_mut() {
                            let interact_span = debug::interact(id);
                            let mut window_events = vec![];

                            events.retain(|(window_id, event)| {
                                if *window_id == id {
                                    window_events.push(event.clone());
                                    false
                                } else {
                                    true
                                }
                            });

                            if window_events.is_empty() {
                                continue;
                            }

                            let (ui_state, statuses) = user_interfaces
                                .get_mut(&id)
                                .expect("Get user interface")
                                .update(
                                    &window_events,
                                    window.state.cursor(),
                                    &mut window.renderer,
                                    &mut clipboard,
                                    &mut messages,
                                );

                            #[cfg(feature = "accessibility")]
                            if let Some(state) = accessibility.get_mut(&id)
                                && state.state.is_active()
                                && state.adapter.is_enabled()
                            {
                                use crate::core::accessibility::NodeId;
                                use crate::core::keyboard;
                                use crate::core::keyboard::key::Named;
                                use crate::core::widget::operation;
                                use std::sync::{Arc, Mutex};

                                // In accessibility mode, Tab/Shift-Tab navigates the VO focus
                                // through all accessibility nodes, NOT the widget focus.
                                let mut handled_tab = false;
                                let mut handled_click = false;

                                for event in window_events.iter() {
                                    // Handle Tab navigation
                                    if let crate::core::Event::Keyboard(
                                        keyboard::Event::KeyPressed { key, modifiers, .. },
                                    ) = event
                                    {
                                        if *key == keyboard::Key::Named(Named::Tab) {
                                            if let Some(ui) = user_interfaces.get_mut(&id) {
                                                let current_focus = state.state.effective_focus();

                                                let output: Arc<
                                                    Mutex<
                                                        Option<
                                                            operation::accessibility::A11yNavigation,
                                                        >,
                                                    >,
                                                > = Arc::new(Mutex::new(None));

                                                if modifiers.shift() {
                                                    let output_ref = Arc::clone(&output);
                                                    let mut op = operation::map(
                                                        operation::accessibility::focus_previous_a11y_node(
                                                            current_focus,
                                                        ),
                                                        move |nav| {
                                                            *output_ref.lock().expect("lock nav") =
                                                                Some(nav);
                                                        },
                                                    );
                                                    ui.operate(&window.renderer, &mut op);
                                                    let _ = op.finish();
                                                } else {
                                                    let output_ref = Arc::clone(&output);
                                                    let mut op = operation::map(
                                                        operation::accessibility::focus_next_a11y_node(
                                                            current_focus,
                                                        ),
                                                        move |nav| {
                                                            *output_ref.lock().expect("lock nav") =
                                                                Some(nav);
                                                        },
                                                    );
                                                    ui.operate(&window.renderer, &mut op);
                                                    let _ = op.finish();
                                                }

                                                if let Some(nav) =
                                                    output.lock().expect("lock nav").take()
                                                {
                                                    if let Some(new_focus) = nav.new_focus {
                                                        if crate::accessibility::trace_enabled() {
                                                            eprintln!(
                                                                "[a11y] Tab navigation: {:?} -> {:?} (total={})",
                                                                current_focus,
                                                                new_focus,
                                                                nav.total_nodes
                                                            );
                                                        }

                                                        // Set both a11y focus AND widget focus
                                                        // Widget focus is needed for cursor rendering etc.
                                                        state.state.set_a11y_focus(new_focus);

                                                        let mut focus_op: Box<
                                                            dyn Operation<()>,
                                                        > = Box::new(
                                                            operation::accessibility::focus_widget_by_node_id(
                                                                new_focus,
                                                            ),
                                                        );
                                                        ui.operate(
                                                            &window.renderer,
                                                            focus_op.as_mut(),
                                                        );

                                                        update_accessibility_tree(
                                                            id, window, ui, state,
                                                        );

                                                        // Deliver a focus event to widgets.
                                                        //
                                                        // In accessibility mode, Tab navigation updates focus
                                                        // programmatically (not via ActionRequested), so widgets
                                                        // won't receive an Accessibility(Focus) event unless we
                                                        // explicitly dispatch it.
                                                        let focus_event =
                                                            core::Event::Accessibility(
                                                                crate::core::accessibility::Event {
                                                                    action:
                                                                        accesskit::Action::Focus,
                                                                    target: new_focus,
                                                                    data: None,
                                                                },
                                                            );

                                                        let focus_events = [focus_event];
                                                        let (focus_ui_state, _focus_statuses) = ui
                                                            .update(
                                                                &focus_events,
                                                                window.state.cursor(),
                                                                &mut window.renderer,
                                                                &mut clipboard,
                                                                &mut messages,
                                                            );

                                                        if let user_interface::State::Updated {
                                                            redraw_request,
                                                            #[cfg(feature = "accessibility")]
                                                            a11y_focus_request,
                                                            ..
                                                        } = focus_ui_state
                                                        {
                                                            // Allow widgets to request a11y focus (e.g. MenuBar opens -> focus first item)
                                                            #[cfg(feature = "accessibility")]
                                                            if let Some(target) = a11y_focus_request
                                                            {
                                                                state.state.set_a11y_focus(target);
                                                                update_accessibility_tree(
                                                                    id, window, ui, state,
                                                                );
                                                            }

                                                            window.request_redraw(redraw_request);
                                                        }
                                                        handled_tab = true;
                                                    }
                                                }
                                            }

                                            break;
                                        }
                                    }

                                    // Handle mouse click - update VO focus to clicked widget
                                    if let crate::core::Event::Mouse(
                                        mouse::Event::ButtonPressed {
                                            button: mouse::Button::Left,
                                            ..
                                        },
                                    ) = event
                                    {
                                        if let mouse::Cursor::Available(position) =
                                            window.state.cursor()
                                        {
                                            if let Some(ui) = user_interfaces.get_mut(&id) {
                                                let output: Arc<Mutex<Option<Option<NodeId>>>> =
                                                    Arc::new(Mutex::new(None));

                                                let output_ref = Arc::clone(&output);
                                                let mut op = operation::map(
                                                    operation::accessibility::find_a11y_node_at_position(
                                                        position,
                                                    ),
                                                    move |found| {
                                                        *output_ref.lock().expect("lock found") =
                                                            Some(found);
                                                    },
                                                );
                                                ui.operate(&window.renderer, &mut op);
                                                let _ = op.finish();

                                                if let Some(Some(new_focus)) =
                                                    output.lock().expect("lock found").take()
                                                {
                                                    if crate::accessibility::trace_enabled() {
                                                        eprintln!(
                                                            "[a11y] Click navigation: {:?} -> {:?}",
                                                            state.state.effective_focus(),
                                                            new_focus
                                                        );
                                                    }

                                                    // Set both a11y focus AND widget focus
                                                    state.state.set_a11y_focus(new_focus);

                                                    let mut focus_op: Box<dyn Operation<()>> =
                                                        Box::new(
                                                            operation::accessibility::focus_widget_by_node_id(
                                                                new_focus,
                                                            ),
                                                        );
                                                    ui.operate(&window.renderer, focus_op.as_mut());

                                                    update_accessibility_tree(
                                                        id, window, ui, state,
                                                    );
                                                    handled_click = true;
                                                }
                                            }
                                        }
                                    }
                                }

                                // Keep the accessibility tree in sync for other events
                                if !handled_tab && !handled_click {
                                    if let Some(ui) = user_interfaces.get_mut(&id) {
                                        update_accessibility_tree(id, window, ui, state);
                                    }
                                }
                            }

                            #[cfg(feature = "unconditional-rendering")]
                            window.request_redraw(window::RedrawRequest::NextFrame);

                            match ui_state {
                                user_interface::State::Updated {
                                    redraw_request: _redraw_request,
                                    mouse_interaction,
                                    #[cfg(target_os = "macos")]
                                    context_menu_request,
                                    #[cfg(feature = "accessibility")]
                                    a11y_focus_request,
                                    ..
                                } => {
                                    // Handle programmatic accessibility focus request from widgets
                                    #[cfg(feature = "accessibility")]
                                    if let Some(target) = a11y_focus_request {
                                        if let Some(acc_state) = accessibility.get_mut(&id) {
                                            acc_state.state.set_a11y_focus(target);
                                            // Update the accessibility tree immediately to reflect the new focus
                                            if let Some(ui) = user_interfaces.get_mut(&id) {
                                                update_accessibility_tree(
                                                    id, window, ui, acc_state,
                                                );
                                            }
                                        }
                                    }

                                    window.update_mouse(mouse_interaction);

                                    #[cfg(not(feature = "unconditional-rendering"))]
                                    window.request_redraw(_redraw_request);

                                    // Handle native context menu request from widget
                                    #[cfg(target_os = "macos")]
                                    if let Some(ctx_req) = context_menu_request {
                                        if let Some(ctx_menu) = &mac_context_menu {
                                            use winit::raw_window_handle::{
                                                HasWindowHandle, RawWindowHandle,
                                            };
                                            if let Ok(handle) = window.raw.window_handle() {
                                                if let RawWindowHandle::AppKit(appkit_handle) =
                                                    handle.as_raw()
                                                {
                                                    let ns_view_ptr = appkit_handle.ns_view.as_ptr()
                                                        as *mut std::ffi::c_void;

                                                    // Position is passed in iced coordinates (top-left origin).
                                                    // The macOS menu function handles the Y-flip internally.
                                                    #[allow(unsafe_code)]
                                                    if let Some(selected_id) = unsafe {
                                                        ctx_menu.show_items_and_wait(
                                                            &ctx_req.items,
                                                            ns_view_ptr,
                                                            f64::from(ctx_req.position.x),
                                                            f64::from(ctx_req.position.y),
                                                        )
                                                    } {
                                                        let ctx_event =
                                                            core::Event::ContextMenuItemSelected(
                                                                selected_id,
                                                            );

                                                        // Process the event through the UI immediately
                                                        // so the ContextMenu widget can translate the
                                                        // MenuId into the corresponding app message.
                                                        if let Some(ui) =
                                                            user_interfaces.get_mut(&id)
                                                        {
                                                            let (_, _) = ui.update(
                                                                &[ctx_event.clone()],
                                                                window.state.cursor(),
                                                                &mut window.renderer,
                                                                &mut clipboard,
                                                                &mut messages,
                                                            );
                                                        }

                                                        events.push((id, ctx_event));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                user_interface::State::Outdated => {
                                    uis_stale = true;
                                }
                            }

                            // Handle automatic Tab focus navigation.
                            //
                            // We purposely do NOT require `Status::Ignored` here because widgets
                            // may conservatively report `Captured` for key events.
                            //
                            // If an application wants to handle Tab itself (e.g. text editors
                            // using Tab for indentation), it can set `FocusLevel::Manual`.
                            //
                            // In accessibility mode, Tab is handled by the VO navigation above,
                            // so we skip this normal focus handling.
                            // We also check adapter.is_enabled() because the Deactivated event
                            // might not always be received (e.g., when VoiceOver is turned off
                            // system-wide on macOS).
                            #[cfg(feature = "accessibility")]
                            let a11y_active = accessibility.get(&id).is_some_and(|state| {
                                state.state.is_active() && state.adapter.is_enabled()
                            });

                            #[cfg(not(feature = "accessibility"))]
                            let a11y_active = false;

                            if !a11y_active && focus_level != FocusLevel::Manual {
                                for event in window_events.iter() {
                                    if let core::Event::Keyboard(
                                        core::keyboard::Event::KeyPressed {
                                            key:
                                                core::keyboard::Key::Named(
                                                    core::keyboard::key::Named::Tab,
                                                ),
                                            modifiers,
                                            ..
                                        },
                                    ) = event
                                    {
                                        #[cfg(feature = "accessibility")]
                                        if crate::accessibility::trace_enabled() {
                                            eprintln!(
                                                "[a11y] Tab pressed (normal mode): shift={}",
                                                modifiers.shift()
                                            );
                                        }

                                        let ui = user_interfaces
                                            .get_mut(&id)
                                            .expect("Get user interface");

                                        // NOTE: `UserInterface::operate` only performs a single traversal.
                                        // Focus operations rely on `Operation::finish()` to chain the
                                        // counting pass into an applying pass.
                                        let mut current_operation: Option<
                                            Box<dyn core::widget::Operation>,
                                        > = Some(if modifiers.shift() {
                                            Box::new(
                                                    core::widget::operation::focusable::focus_previous_filtered(
                                                        focus_level,
                                                    ),
                                                )
                                        } else {
                                            Box::new(
                                                    core::widget::operation::focusable::focus_next_filtered(
                                                        focus_level,
                                                    ),
                                                )
                                        });

                                        while let Some(mut operation) = current_operation.take() {
                                            ui.operate(&window.renderer, operation.as_mut());

                                            match operation.finish() {
                                                core::widget::operation::Outcome::None => {}
                                                core::widget::operation::Outcome::Some(()) => {}
                                                core::widget::operation::Outcome::Chain(next) => {
                                                    current_operation = Some(next);
                                                }
                                            }
                                        }

                                        // Ensure the focus change becomes visible immediately.
                                        #[cfg(feature = "unconditional-rendering")]
                                        window.request_redraw(window::RedrawRequest::NextFrame);

                                        #[cfg(not(feature = "unconditional-rendering"))]
                                        window.request_redraw(window::RedrawRequest::NextFrame);
                                    }
                                }
                            }

                            for (event, status) in
                                window_events.into_iter().zip(statuses.into_iter())
                            {
                                runtime.broadcast(subscription::Event::Interaction {
                                    window: id,
                                    event,
                                    status,
                                });
                            }

                            interact_span.finish();
                        }

                        for (id, event) in events.drain(..) {
                            runtime.broadcast(subscription::Event::Interaction {
                                window: id,
                                event,
                                status: core::event::Status::Ignored,
                            });
                        }

                        if !messages.is_empty() || uis_stale {
                            let cached_interfaces: FxHashMap<_, _> =
                                ManuallyDrop::into_inner(user_interfaces)
                                    .into_iter()
                                    .map(|(id, ui)| (id, ui.into_cache()))
                                    .collect();

                            let actions = update(&mut program, &mut runtime, &mut messages);

                            user_interfaces = ManuallyDrop::new(build_user_interfaces(
                                &program,
                                &mut window_manager,
                                cached_interfaces,
                            ));

                            for action in actions {
                                run_action(
                                    action,
                                    &program,
                                    &mut runtime,
                                    &mut compositor,
                                    &mut events,
                                    &mut messages,
                                    &mut clipboard,
                                    &mut dnd_manager,
                                    &mut control_sender,
                                    &mut user_interfaces,
                                    &mut window_manager,
                                    &mut ui_caches,
                                    &mut is_window_opening,
                                    #[cfg(feature = "accessibility")]
                                    &mut accessibility,
                                    &mut system_theme,
                                );
                            }

                            for (_id, window) in window_manager.iter_mut() {
                                window.raw.request_redraw();
                            }
                        }

                        if let Some(redraw_at) = window_manager.redraw_at() {
                            let _ = control_sender
                                .start_send(Control::ChangeFlow(ControlFlow::WaitUntil(redraw_at)));
                        } else {
                            let _ =
                                control_sender.start_send(Control::ChangeFlow(ControlFlow::Wait));
                        }
                    }
                    _ => {}
                }
            }
            Event::Exit => break,
        }
    }

    let _ = ManuallyDrop::into_inner(user_interfaces);
}

#[cfg(feature = "accessibility")]
fn update_accessibility_tree<'a, P, C>(
    _id: window::Id,
    window: &mut window::Window<P, C>,
    ui: &mut UserInterface<'a, P::Message, P::Theme, P::Renderer>,
    state: &mut AccessibilityState,
) where
    P: Program,
    C: Compositor<Renderer = P::Renderer> + 'static,
    P::Theme: theme::Base,
{
    use crate::core::accessibility::{Node, NodeId, Role};
    use crate::core::widget::operation;
    use crate::core::widget::operation::Operation;
    use accesskit::Live;
    use std::sync::{Arc, Mutex};

    if !state.state.is_active() {
        return;
    }

    if crate::accessibility::trace_enabled() {
        eprintln!(
            "[a11y] update_accessibility_tree window={:?} adapter_enabled={} ",
            _id,
            state.adapter.is_enabled()
        );
    }

    // AccessKit expects coordinates in physical pixels on all platforms.
    // See: https://github.com/AccessKit/accesskit - "AccessKit expects the final
    // transformed coordinates to be relative to the origin of the tree's container
    // (e.g. window), in physical pixels, with the y coordinate being top-down."
    let (bounds, scale_factor) = {
        let physical_size = window.state.physical_size();
        let bounds = crate::core::Rectangle {
            x: 0.0,
            y: 0.0,
            width: physical_size.width as f32,
            height: physical_size.height as f32,
        };
        (bounds, window.state.scale_factor() as f64)
    };

    let collected = {
        let output: Arc<Mutex<Option<operation::AccessibilityTree>>> = Arc::new(Mutex::new(None));
        let output_ref = Arc::clone(&output);

        let mut op = operation::map(operation::accessibility::collect(), move |tree| {
            *output_ref.lock().expect("lock accessibility tree") = Some(tree);
        });

        ui.operate(&window.renderer, &mut op);
        let _ = op.finish();

        output.lock().expect("lock accessibility tree").take()
    };

    let Some(collected) = collected else {
        if crate::accessibility::trace_enabled() {
            eprintln!("[a11y] collect() produced no tree");
        }
        return;
    };

    if crate::accessibility::trace_enabled() {
        eprintln!("[a11y] collected nodes_len={}", collected.nodes.len());
    }

    let root_id = crate::accessibility::AccessibilityAdapter::ROOT_ID;
    let announcer_id = NodeId(1);

    let mut root = crate::accessibility::create_window_node("", bounds);

    let mut announcer = Node::new(Role::Label);
    announcer.set_bounds(accesskit::Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    });
    announcer.set_live(Live::Off);

    if let Some((message, priority)) = state.state.take_announcement() {
        let serial = state.state.next_announcement_serial();

        // Ensure the value changes even if the same message is repeated.
        let label = format!("{message} ({serial})");
        announcer.set_label(label);

        announcer.set_live(match priority {
            crate::core::accessibility::AnnouncementPriority::Polite => Live::Polite,
            crate::core::accessibility::AnnouncementPriority::Assertive => Live::Assertive,
        });
    }

    let mut children = Vec::with_capacity(1 + collected.top_level_ids.len());
    children.push(announcer_id);
    // Deduplicate child IDs - some widgets may generate the same path-based ID
    // if they don't have explicit widget IDs. Use top_level_ids which excludes
    // extra generated children (like TextRun) that are nested inside widgets.
    {
        let mut seen = std::collections::HashSet::new();
        let _ = seen.insert(announcer_id);
        for id in collected.top_level_ids.iter() {
            if seen.insert(*id) {
                children.push(*id);
            } else if crate::accessibility::trace_enabled() {
                eprintln!(
                    "[a11y] WARNING: duplicate NodeId {:?} in children, skipping",
                    id
                );
            }
        }
    }
    root.set_children(children);

    // Get the effective focus - prefer a11y focus, then fall back to widget focus
    let focus = state.state.a11y_focus.node.or_else(|| {
        let output: Arc<Mutex<Option<NodeId>>> = Arc::new(Mutex::new(None));
        let output_ref = Arc::clone(&output);

        let mut op = operation::map(operation::accessibility::find_focused_node(), move |id| {
            *output_ref.lock().expect("lock focused node") = Some(id);
        });

        ui.operate(&window.renderer, &mut op);
        let _ = op.finish();

        let focused = output.lock().expect("lock focused node").take();

        // Update the state's widget focus (used as fallback for effective focus).
        state.state.set_widget_focus(focused);

        focused
    });

    let focus = focus.or_else(|| collected.nodes.first().map(|(id, _)| *id));

    if crate::accessibility::trace_enabled() {
        eprintln!("[a11y] effective focus={:?}", focus);
    }

    // Detect focus change and announce the newly focused widget's label/value.
    if focus != state.last_focused {
        state.last_focused = focus;

        // If there's a newly focused node, find its label/value from collected nodes and announce it.
        if let Some(focused_id) = focus {
            if let Some((_, node)) = collected.nodes.iter().find(|(id, _)| *id == focused_id) {
                // Try to derive a description from the node.
                // AccessKit Node has label() and value() accessors.
                let label_text = node.label().map(|s| s.to_string());
                let value_text = node.value().map(|s| s.to_string());
                let role_text = format!("{:?}", node.role());

                // Combine role + label/value into announcement.
                let announcement = match (label_text, value_text) {
                    (Some(lbl), Some(val)) if !val.is_empty() => {
                        format!("{role_text}: {lbl}, {val}")
                    }
                    (Some(lbl), _) => format!("{role_text}: {lbl}"),
                    (None, Some(val)) if !val.is_empty() => format!("{role_text}: {val}"),
                    _ => role_text,
                };

                // Use existing announcer mechanism.
                let serial = state.state.next_announcement_serial();
                let label = format!("{announcement} ({serial})");
                announcer.set_label(label);
                announcer.set_live(Live::Polite);
            }
        }
    }

    let mut nodes = Vec::with_capacity(2 + collected.nodes.len());
    nodes.push((root_id, root));
    nodes.push((announcer_id, announcer));

    // Track which NodeIds we've already added to avoid duplicates
    let mut seen_node_ids = std::collections::HashSet::new();
    let _ = seen_node_ids.insert(root_id);
    let _ = seen_node_ids.insert(announcer_id);

    // Scale bounds using the platform-specific scale factor determined above.
    // On macOS: physical pixels (scale_factor from system)
    // On other platforms: logical pixels (scale_factor = 1.0)
    for (node_id, mut node) in collected.nodes {
        // Skip duplicate NodeIds
        if !seen_node_ids.insert(node_id) {
            if crate::accessibility::trace_enabled() {
                eprintln!(
                    "[a11y] WARNING: duplicate NodeId {:?} in nodes, skipping",
                    node_id
                );
            }
            continue;
        }

        // Scale the bounds if they exist
        if let Some(bounds) = node.bounds() {
            node.set_bounds(accesskit::Rect {
                x0: bounds.x0 * scale_factor,
                y0: bounds.y0 * scale_factor,
                x1: bounds.x1 * scale_factor,
                y1: bounds.y1 * scale_factor,
            });
        }
        nodes.push((node_id, node));
    }

    // Verify all child references are valid before sending to AccessKit
    if crate::accessibility::trace_enabled() {
        let node_id_set: std::collections::HashSet<_> = nodes.iter().map(|(id, _)| *id).collect();
        for (nid, node) in &nodes {
            for child_id in node.children() {
                if !node_id_set.contains(child_id) {
                    eprintln!(
                        "[a11y] ERROR: node {:?} references child {:?} which does not exist in tree!",
                        nid, child_id
                    );
                }
            }
        }
    }

    // Validate that focus exists in the node list - AccessKit will panic if it doesn't
    let node_id_set: std::collections::HashSet<_> = nodes.iter().map(|(id, _)| *id).collect();
    let validated_focus = if let Some(focus_id) = focus {
        if node_id_set.contains(&focus_id) {
            Some(focus_id)
        } else {
            if crate::accessibility::trace_enabled() {
                eprintln!(
                    "[a11y] WARNING: focus {:?} not in node list, falling back to root",
                    focus_id
                );
            }
            // Clear the invalid a11y focus to prevent repeated warnings
            state.state.a11y_focus.clear();
            None
        }
    } else {
        None
    };

    state.adapter.update_tree(nodes, validated_focus);

    if crate::accessibility::trace_enabled() {
        eprintln!("[a11y] update_accessibility_tree done");
    }
}

/// Builds a window's [`UserInterface`] for the [`Program`].
#[cfg(target_os = "macos")]
fn build_user_interface<'a, P: Program>(
    program: &'a program::Instance<P>,
    cache: user_interface::Cache,
    renderer: &mut P::Renderer,
    size: Size,
    menu_context: &core::menu::MenuContext,
    id: window::Id,
) -> UserInterface<'a, P::Message, P::Theme, P::Renderer>
where
    P::Theme: theme::Base,
{
    let view_span = debug::view(id);

    let _ = menu_context;
    let view = program.view(id);
    view_span.finish();

    let layout_span = debug::layout(id);
    let user_interface = UserInterface::build(view, size, cache, renderer);
    layout_span.finish();

    user_interface
}

/// Builds a window's [`UserInterface`] for the [`Program`].
///
/// On non-macOS platforms, this function also renders the application menu bar
/// as part of the UI.
#[cfg(not(target_os = "macos"))]
fn build_user_interface<'a, P: Program>(
    program: &'a program::Instance<P>,
    cache: user_interface::Cache,
    renderer: &mut P::Renderer,
    size: Size,
    menu_context: &core::menu::MenuContext,
    id: window::Id,
) -> UserInterface<'a, P::Message, P::Theme, P::Renderer>
where
    P: Program<Theme = icy_ui_widget::Theme, Renderer = icy_ui_widget::Renderer>,
    P::Theme: theme::Base,
    P::Message: Clone,
{
    let view_span = debug::view(id);

    let mut view = program.view(id);

    // Create a menu context with the current window set to this window's id
    let window_menu_context = core::menu::MenuContext {
        current_window: Some(id),
        windows: menu_context.windows.clone(),
    };

    if let Some(menu) = program.application_menu(&window_menu_context) {
        use icy_ui_widget::menu::menu_bar_from;
        use icy_ui_widget::{Column, core::Length};

        let menu_bar = menu_bar_from(&menu);

        view = Column::new()
            .push(menu_bar)
            .push(view)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    view_span.finish();

    let layout_span = debug::layout(id);
    let user_interface = UserInterface::build(view, size, cache, renderer);
    layout_span.finish();

    user_interface
}

#[cfg(target_os = "macos")]
mod macos_menu {
    use super::*;
    use crate::core::menu;

    pub fn menu_for_platform<Message>(
        menu_model: menu::AppMenu<Message>,
    ) -> (
        menu::AppMenu<menu::MenuId>,
        FxHashMap<menu::MenuId, Message>,
        u64,
    ) {
        let mut actions: FxHashMap<menu::MenuId, Message> = FxHashMap::default();

        let roots = menu_model
            .roots
            .into_iter()
            .map(|node| convert_node(node, &mut actions))
            .collect();

        let menu_for_platform = menu::AppMenu::new(roots);
        let signature = signature(&menu_for_platform);

        (menu_for_platform, actions, signature)
    }

    fn convert_node<Message>(
        node: menu::MenuNode<Message>,
        actions: &mut FxHashMap<menu::MenuId, Message>,
    ) -> menu::MenuNode<menu::MenuId> {
        let id = node.id;
        let role = node.role;

        match node.kind {
            menu::MenuKind::Separator => menu::MenuNode {
                id,
                role,
                kind: menu::MenuKind::Separator,
            },

            menu::MenuKind::Submenu {
                label,
                enabled,
                children,
            } => menu::MenuNode {
                id,
                role,
                kind: menu::MenuKind::Submenu {
                    label,
                    enabled,
                    children: children
                        .into_iter()
                        .map(|child| convert_node(child, actions))
                        .collect(),
                },
            },

            menu::MenuKind::Item {
                label,
                enabled,
                shortcut,
                on_activate,
            } => {
                let _ = actions.insert(id.clone(), on_activate);

                menu::MenuNode {
                    id: id.clone(),
                    role,
                    kind: menu::MenuKind::Item {
                        label,
                        enabled,
                        shortcut,
                        on_activate: id,
                    },
                }
            }

            menu::MenuKind::CheckItem {
                label,
                enabled,
                checked,
                shortcut,
                on_activate,
            } => {
                let _ = actions.insert(id.clone(), on_activate);

                menu::MenuNode {
                    id: id.clone(),
                    role,
                    kind: menu::MenuKind::CheckItem {
                        label,
                        enabled,
                        checked,
                        shortcut,
                        on_activate: id,
                    },
                }
            }
        }
    }

    fn signature(menu: &menu::AppMenu<menu::MenuId>) -> u64 {
        use std::hash::Hasher;

        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        for root in &menu.roots {
            hash_node(root, &mut hasher);
        }

        hasher.finish()
    }

    fn hash_node(node: &menu::MenuNode<menu::MenuId>, hasher: &mut impl std::hash::Hasher) {
        use std::hash::Hash;

        node.id.hash(hasher);

        match &node.kind {
            menu::MenuKind::Separator => {
                0u8.hash(hasher);
            }
            menu::MenuKind::Item {
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
            menu::MenuKind::CheckItem {
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
            menu::MenuKind::Submenu {
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
}

fn update<P: Program, E: Executor>(
    program: &mut program::Instance<P>,
    runtime: &mut Runtime<E, Proxy<P::Message>, Action<P::Message>>,
    messages: &mut Vec<P::Message>,
) -> Vec<Action<P::Message>>
where
    P::Theme: theme::Base,
{
    use futures::futures;

    let mut actions = Vec::new();

    for message in messages.drain(..) {
        let task = runtime.enter(|| program.update(message));

        if let Some(mut stream) = runtime::task::into_stream(task) {
            let waker = futures::task::noop_waker_ref();
            let mut context = futures::task::Context::from_waker(waker);

            // Run immediately available actions synchronously (e.g. widget operations)
            loop {
                match runtime.enter(|| stream.poll_next_unpin(&mut context)) {
                    futures::task::Poll::Ready(Some(action)) => {
                        actions.push(action);
                    }
                    futures::task::Poll::Ready(None) => {
                        break;
                    }
                    futures::task::Poll::Pending => {
                        runtime.run(stream);
                        break;
                    }
                }
            }
        }
    }

    let subscription = runtime.enter(|| program.subscription());
    let recipes = subscription::into_recipes(subscription.map(Action::Output));

    runtime.track(recipes);

    actions
}

fn run_action<'a, P, C>(
    action: Action<P::Message>,
    program: &'a program::Instance<P>,
    runtime: &mut Runtime<P::Executor, Proxy<P::Message>, Action<P::Message>>,
    compositor: &mut Option<C>,
    events: &mut Vec<(window::Id, core::Event)>,
    messages: &mut Vec<P::Message>,
    clipboard: &mut Clipboard,
    dnd_manager: &mut DndManager,
    control_sender: &mut mpsc::UnboundedSender<Control>,
    interfaces: &mut FxHashMap<window::Id, UserInterface<'a, P::Message, P::Theme, P::Renderer>>,
    window_manager: &mut WindowManager<P, C>,
    ui_caches: &mut FxHashMap<window::Id, user_interface::Cache>,
    is_window_opening: &mut bool,
    #[cfg(feature = "accessibility")] accessibility: &mut FxHashMap<window::Id, AccessibilityState>,
    system_theme: &mut theme::Mode,
) where
    P: Program<Theme = icy_ui_widget::Theme, Renderer = icy_ui_widget::Renderer>,
    C: Compositor<Renderer = P::Renderer> + 'static,
    P::Theme: theme::Base,
    P::Message: Clone,
{
    use crate::core::Renderer as _;
    use crate::runtime::clipboard;
    use crate::runtime::window;
    use std::borrow::Cow;

    match action {
        Action::Output(message) => {
            messages.push(message);
        }
        Action::Clipboard(action) => match action {
            clipboard::Action::ReadText { target, channel } => {
                let _ = channel.send(clipboard.read_text(target));
            }
            clipboard::Action::WriteText { target, contents } => {
                clipboard.write_text(target, contents);
            }
            clipboard::Action::Read {
                target,
                formats,
                channel,
            } => {
                let format_refs: Vec<&str> = formats.iter().map(String::as_str).collect();
                let _ = channel.send(clipboard.read(target, &format_refs));
            }
            clipboard::Action::Write {
                target,
                data,
                formats,
            } => {
                let format_refs: Vec<&str> = formats.iter().map(String::as_str).collect();
                clipboard.write(target, Cow::Owned(data), &format_refs);
            }
            clipboard::Action::WriteMulti { target, formats } => {
                // Keep the original strings alive while we build the references
                let formats_owned: Vec<(Vec<u8>, Vec<String>)> = formats;
                let formats_with_refs: Vec<(Cow<'_, [u8]>, Vec<&str>)> = formats_owned
                    .iter()
                    .map(|(data, mimes)| {
                        (
                            Cow::Borrowed(data.as_slice()),
                            mimes.iter().map(String::as_str).collect(),
                        )
                    })
                    .collect();
                let format_slices: Vec<(Cow<'_, [u8]>, &[&str])> = formats_with_refs
                    .iter()
                    .map(|(data, mimes)| (data.clone(), mimes.as_slice()))
                    .collect();
                clipboard.write_multi(target, &format_slices);
            }
            clipboard::Action::AvailableFormats { target, channel } => {
                let _ = channel.send(clipboard.available_formats(target));
            }
            clipboard::Action::ReadFiles { target, channel } => {
                let _ = channel.send(clipboard.read_files(target));
            }
            clipboard::Action::WriteFiles { target, paths } => {
                clipboard.write_files(target, &paths);
            }
            clipboard::Action::Clear { target } => {
                clipboard.clear(target);
            }
            clipboard::Action::ReadAll {
                target,
                formats,
                channel,
            } => {
                let format_refs: Vec<&str> = formats.iter().map(String::as_str).collect();
                let _ = channel.send(clipboard.read_all(target, &format_refs));
            }
        },
        Action::Dnd(action) => {
            // DnD actions are processed by the DndManager
            dnd_manager.process(action);
        }
        Action::Window(action) => match action {
            window::Action::Open(id, settings, channel) => {
                let monitor = window_manager.last_monitor();

                control_sender
                    .start_send(Control::CreateWindow {
                        id,
                        settings,
                        title: program.title(id),
                        scale_factor: program.scale_factor(id),
                        monitor,
                        on_open: channel,
                    })
                    .expect("Send control action");

                *is_window_opening = true;
            }
            window::Action::Close(id) => {
                let _ = ui_caches.remove(&id);
                let _ = interfaces.remove(&id);

                #[cfg(feature = "accessibility")]
                {
                    let _ = accessibility.remove(&id);
                }

                if let Some(window) = window_manager.remove(id) {
                    if clipboard.window_id() == Some(window.raw.id()) {
                        *clipboard = window_manager
                            .first()
                            .map(|window| window.raw.clone())
                            .map(Clipboard::connect)
                            .unwrap_or_else(Clipboard::unconnected);
                    }

                    events.push((id, core::Event::Window(core::window::Event::Closed)));
                }

                if window_manager.is_empty() {
                    *compositor = None;
                }
            }
            window::Action::GetOldest(channel) => {
                let id = window_manager.iter_mut().next().map(|(id, _window)| id);

                let _ = channel.send(id);
            }
            window::Action::GetLatest(channel) => {
                let id = window_manager.iter_mut().last().map(|(id, _window)| id);

                let _ = channel.send(id);
            }
            window::Action::Drag(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = window.raw.drag_window();
                }
            }
            window::Action::DragResize(id, direction) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = window
                        .raw
                        .drag_resize_window(conversion::resize_direction(direction));
                }
            }
            window::Action::Resize(id, size) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = window.raw.request_inner_size(
                        winit::dpi::LogicalSize {
                            width: size.width,
                            height: size.height,
                        }
                        .to_physical::<f32>(f64::from(window.state.scale_factor())),
                    );
                }
            }
            window::Action::SetMinSize(id, size) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_min_inner_size(size.map(|size| {
                        winit::dpi::LogicalSize {
                            width: size.width,
                            height: size.height,
                        }
                        .to_physical::<f32>(f64::from(window.state.scale_factor()))
                    }));
                }
            }
            window::Action::SetMaxSize(id, size) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_max_inner_size(size.map(|size| {
                        winit::dpi::LogicalSize {
                            width: size.width,
                            height: size.height,
                        }
                        .to_physical::<f32>(f64::from(window.state.scale_factor()))
                    }));
                }
            }
            window::Action::SetResizeIncrements(id, increments) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_resize_increments(increments.map(|size| {
                        winit::dpi::LogicalSize {
                            width: size.width,
                            height: size.height,
                        }
                        .to_physical::<f32>(f64::from(window.state.scale_factor()))
                    }));
                }
            }
            window::Action::SetResizable(id, resizable) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_resizable(resizable);
                }
            }
            window::Action::GetSize(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let size = window.logical_size();
                    let _ = channel.send(Size::new(size.width, size.height));
                }
            }
            window::Action::GetMaximized(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = channel.send(window.raw.is_maximized());
                }
            }
            window::Action::Maximize(id, maximized) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_maximized(maximized);
                }
            }
            window::Action::GetMinimized(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = channel.send(window.raw.is_minimized());
                }
            }
            window::Action::Minimize(id, minimized) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_minimized(minimized);
                }
            }
            window::Action::GetPosition(id, channel) => {
                if let Some(window) = window_manager.get(id) {
                    let position = window
                        .raw
                        .outer_position()
                        .map(|position| {
                            let position = position.to_logical::<f32>(window.raw.scale_factor());

                            Point::new(position.x, position.y)
                        })
                        .ok();

                    let _ = channel.send(position);
                }
            }
            window::Action::GetScaleFactor(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let scale_factor = window.raw.scale_factor();

                    let _ = channel.send(scale_factor as f32);
                }
            }
            window::Action::Move(id, position) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_outer_position(winit::dpi::LogicalPosition {
                        x: position.x,
                        y: position.y,
                    });
                }
            }
            window::Action::SetMode(id, mode) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_visible(conversion::visible(mode));
                    window
                        .raw
                        .set_fullscreen(conversion::fullscreen(window.raw.current_monitor(), mode));
                }
            }
            window::Action::SetIcon(id, icon) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_window_icon(conversion::icon(icon));
                }
            }
            window::Action::GetMode(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let mode = if window.raw.is_visible().unwrap_or(true) {
                        conversion::mode(window.raw.fullscreen())
                    } else {
                        core::window::Mode::Hidden
                    };

                    let _ = channel.send(mode);
                }
            }
            window::Action::ToggleMaximize(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_maximized(!window.raw.is_maximized());
                }
            }
            window::Action::ToggleDecorations(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_decorations(!window.raw.is_decorated());
                }
            }
            window::Action::RequestUserAttention(id, attention_type) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window
                        .raw
                        .request_user_attention(attention_type.map(conversion::user_attention));
                }
            }
            window::Action::GainFocus(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.focus_window();
                }
            }
            window::Action::SetLevel(id, level) => {
                if let Some(window) = window_manager.get_mut(id) {
                    window.raw.set_window_level(conversion::window_level(level));
                }
            }
            window::Action::ShowSystemMenu(id) => {
                if let Some(window) = window_manager.get_mut(id)
                    && let mouse::Cursor::Available(point) = window.state.cursor()
                {
                    window.raw.show_window_menu(winit::dpi::LogicalPosition {
                        x: point.x,
                        y: point.y,
                    });
                }
            }
            window::Action::GetRawId(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = channel.send(window.raw.id().into());
                }
            }
            window::Action::Run(id, f) => {
                if let Some(window) = window_manager.get_mut(id) {
                    f(window);
                }
            }
            window::Action::Screenshot(id, channel) => {
                if let Some(window) = window_manager.get_mut(id)
                    && let Some(compositor) = compositor
                {
                    let bytes = compositor.screenshot(
                        &mut window.renderer,
                        window.state.viewport(),
                        window.state.background_color(),
                    );

                    let _ = channel.send(core::window::Screenshot::new(
                        bytes,
                        window.state.physical_size(),
                        window.state.scale_factor(),
                    ));
                }
            }
            window::Action::EnableMousePassthrough(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = window.raw.set_cursor_hittest(false);
                }
            }
            window::Action::DisableMousePassthrough(id) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = window.raw.set_cursor_hittest(true);
                }
            }
            window::Action::GetMonitorSize(id, channel) => {
                if let Some(window) = window_manager.get(id) {
                    let size = window.raw.current_monitor().map(|monitor| {
                        let scale = window.state.scale_factor();
                        let size = monitor.size().to_logical(f64::from(scale));

                        Size::new(size.width, size.height)
                    });

                    let _ = channel.send(size);
                }
            }
            window::Action::SetAllowAutomaticTabbing(enabled) => {
                control_sender
                    .start_send(Control::SetAutomaticWindowTabbing(enabled))
                    .expect("Send control action");
            }
            window::Action::SetLayoutDirection(direction) => {
                core::set_layout_direction(direction);
                for (id, window) in window_manager.iter_mut() {
                    if let Some(ui) = interfaces.remove(&id) {
                        let _ = interfaces.insert(
                            id,
                            ui.relayout(window.state.logical_size(), &mut window.renderer),
                        );
                    }
                    window.raw.request_redraw();
                }
            }
            window::Action::RedrawAll => {
                for (_id, window) in window_manager.iter_mut() {
                    window.raw.request_redraw();
                }
            }
            window::Action::RelayoutAll => {
                for (id, window) in window_manager.iter_mut() {
                    if let Some(ui) = interfaces.remove(&id) {
                        let _ = interfaces.insert(
                            id,
                            ui.relayout(window.state.logical_size(), &mut window.renderer),
                        );
                    }

                    window.raw.request_redraw();
                }
            }
        },
        Action::System(action) => match action {
            system::Action::GetInformation(_channel) => {
                #[cfg(feature = "sysinfo")]
                {
                    if let Some(compositor) = compositor {
                        let graphics_info = compositor.information();

                        let _ = std::thread::spawn(move || {
                            let information = system_information(graphics_info);

                            let _ = _channel.send(information);
                        });
                    }
                }
            }
            system::Action::GetTheme(channel) => {
                let _ = channel.send(*system_theme);
            }
            system::Action::NotifyTheme(mode) => {
                if mode != *system_theme {
                    *system_theme = mode;

                    runtime.broadcast(subscription::Event::SystemThemeChanged(mode));
                }

                let Some(theme) = conversion::window_theme(mode) else {
                    return;
                };

                for (_id, window) in window_manager.iter_mut() {
                    window.state.update(
                        program,
                        &window.raw,
                        &winit::event::WindowEvent::ThemeChanged(theme),
                    );
                }
            }
        },
        Action::Widget(operation) => {
            let mut current_operation = Some(operation);

            while let Some(mut operation) = current_operation.take() {
                for (id, ui) in interfaces.iter_mut() {
                    if let Some(window) = window_manager.get_mut(*id) {
                        ui.operate(&window.renderer, operation.as_mut());
                    }
                }

                match operation.finish() {
                    operation::Outcome::None => {}
                    operation::Outcome::Some(()) => {}
                    operation::Outcome::Chain(next) => {
                        current_operation = Some(next);
                    }
                }
            }
        }
        Action::Image(action) => match action {
            image::Action::Allocate(handle, sender) => {
                // TODO: Shared image cache in compositor
                if let Some((_id, window)) = window_manager.iter_mut().next() {
                    window.renderer.allocate_image(&handle, move |allocation| {
                        let _ = sender.send(allocation);
                    });
                }
            }
        },
        Action::LoadFont { bytes, channel } => {
            if let Some(compositor) = compositor {
                // TODO: Error handling (?)
                compositor.load_font(bytes.clone());

                let _ = channel.send(Ok(()));
            }
        }
        Action::Tick => {
            for (_id, window) in window_manager.iter_mut() {
                window.renderer.tick();
            }
        }
        Action::Reload => {
            let menu_context = {
                let mut windows = Vec::new();

                for (id, window) in window_manager.iter_mut() {
                    windows.push(core::menu::WindowInfo {
                        id,
                        title: window.state.title().to_owned(),
                        focused: window.state.focused(),
                        minimized: window.raw.is_minimized().unwrap_or(false),
                    });
                }

                let focused_window = windows.iter().find(|w| w.focused).map(|w| w.id);
                core::menu::MenuContext {
                    current_window: focused_window,
                    windows,
                }
            };

            for (id, window) in window_manager.iter_mut() {
                let Some(ui) = interfaces.remove(&id) else {
                    continue;
                };

                let cache = ui.into_cache();
                let size = window.logical_size();

                let _ = interfaces.insert(
                    id,
                    build_user_interface(
                        program,
                        cache,
                        &mut window.renderer,
                        size,
                        &menu_context,
                        id,
                    ),
                );

                window.raw.request_redraw();
            }
        }
        #[cfg(feature = "accessibility")]
        Action::Accessibility(action) => {
            use crate::runtime::accessibility::Action as AccessibilityAction;
            match action {
                AccessibilityAction::Announce { message, priority } => {
                    let Some((id, window)) = window_manager.iter_mut().next() else {
                        return;
                    };

                    let Some(state) = accessibility.get_mut(&id) else {
                        return;
                    };

                    log::info!("Screen reader announcement ({:?}): {}", priority, message);
                    // Convert runtime priority to core priority
                    let core_priority = match priority {
                        crate::runtime::accessibility::Priority::Polite => {
                            crate::core::accessibility::AnnouncementPriority::Polite
                        }
                        crate::runtime::accessibility::Priority::Assertive => {
                            crate::core::accessibility::AnnouncementPriority::Assertive
                        }
                    };
                    state.state.announce(message, core_priority);
                    window.raw.request_redraw();
                }
                AccessibilityAction::Focus { target } => {
                    let Some((id, window)) = window_manager.iter_mut().next() else {
                        return;
                    };

                    let Some(state) = accessibility.get_mut(&id) else {
                        return;
                    };

                    log::info!("Focus accessible element: {:?}", target);
                    state.state.set_a11y_focus(target);
                    window.raw.request_redraw();
                }
            }
        }
        Action::Exit => {
            control_sender
                .start_send(Control::Exit)
                .expect("Send control action");
        }
    }
}

/// Build the user interface for every window.
pub fn build_user_interfaces<'a, P: Program, C>(
    program: &'a program::Instance<P>,
    window_manager: &mut WindowManager<P, C>,
    mut cached_user_interfaces: FxHashMap<window::Id, user_interface::Cache>,
) -> FxHashMap<window::Id, UserInterface<'a, P::Message, P::Theme, P::Renderer>>
where
    P: Program<Theme = icy_ui_widget::Theme, Renderer = icy_ui_widget::Renderer>,
    C: Compositor<Renderer = P::Renderer>,
    P::Theme: theme::Base,
    P::Message: Clone,
{
    let mut menu_windows = Vec::new();

    for (id, window) in window_manager.iter_mut() {
        window.state.synchronize(program, id, &window.raw);

        #[cfg(feature = "hinting")]
        {
            use crate::core::Renderer as _;
            window.renderer.hint(window.state.scale_factor());
        }

        menu_windows.push(core::menu::WindowInfo {
            id,
            title: window.state.title().to_owned(),
            focused: window.state.focused(),
            minimized: window.raw.is_minimized().unwrap_or(false),
        });
    }

    let focused_window = menu_windows.iter().find(|w| w.focused).map(|w| w.id);
    let menu_context = core::menu::MenuContext {
        current_window: focused_window,
        windows: menu_windows,
    };

    debug::theme_changed(|| {
        window_manager
            .first()
            .and_then(|window| theme::Base::palette(window.state.theme()))
    });

    cached_user_interfaces
        .drain()
        .filter_map(|(id, cache)| {
            let window = window_manager.get_mut(id)?;

            Some((
                id,
                build_user_interface(
                    program,
                    cache,
                    &mut window.renderer,
                    window.state.logical_size(),
                    &menu_context,
                    id,
                ),
            ))
        })
        .collect()
}

/// Returns true if the provided event should cause a [`Program`] to
/// exit.
pub fn user_force_quit(
    event: &winit::event::WindowEvent,
    _modifiers: winit::keyboard::ModifiersState,
) -> bool {
    match event {
        #[cfg(target_os = "macos")]
        winit::event::WindowEvent::KeyboardInput {
            event:
                winit::event::KeyEvent {
                    logical_key: winit::keyboard::Key::Character(c),
                    state: winit::event::ElementState::Pressed,
                    ..
                },
            ..
        } if c == "q" && _modifiers.super_key() => true,
        _ => false,
    }
}

#[cfg(feature = "sysinfo")]
fn system_information(graphics: compositor::Information) -> system::Information {
    use sysinfo::{Process, System};

    let mut system = System::new_all();
    system.refresh_all();

    let cpu_brand = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().to_string())
        .unwrap_or_default();

    let memory_used = sysinfo::get_current_pid()
        .and_then(|pid| system.process(pid).ok_or("Process not found"))
        .map(Process::memory)
        .ok();

    system::Information {
        system_name: System::name(),
        system_kernel: System::kernel_version(),
        system_version: System::long_os_version(),
        system_short_version: System::os_version(),
        cpu_brand,
        cpu_cores: system.physical_core_count(),
        memory_total: system.total_memory(),
        memory_used,
        graphics_adapter: graphics.adapter,
        graphics_backend: graphics.backend,
    }
}
