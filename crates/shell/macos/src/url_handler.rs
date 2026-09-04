//! URL handler for macOS custom URL schemes and Finder open-document events.
//!
//! This module allows your application to handle custom URL schemes like `myapp://action`.
//! It works by registering an Apple Event handler for `kAEGetURL` events.
//!
//! # Setup
//!
//! 1. Call [`UrlHandler::install`] early in your application (before the event loop starts)
//! 2. Poll the returned receiver for URL events
//! 3. Make sure your app bundle's `Info.plist` declares the URL scheme:
//!
//! ```xml
//! <key>CFBundleURLTypes</key>
//! <array>
//!     <dict>
//!         <key>CFBundleURLName</key>
//!         <string>com.example.myapp</string>
//!         <key>CFBundleURLSchemes</key>
//!         <array>
//!             <string>myapp</string>
//!         </array>
//!     </dict>
//! </array>
//! ```

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol};

/// Apple Event constants for URL and document handling.
mod constants {
    /// The event class for core Apple Events (kCoreEventClass = 'aevt').
    pub const K_CORE_EVENT_CLASS: u32 = 0x6165_7674;
    /// The event ID for opening documents (kAEOpenDocuments = 'odoc').
    pub const K_AE_OPEN_DOCUMENTS: u32 = 0x6f64_6f63;
    /// The event class for internet events (kInternetEventClass = 'GURL').
    pub const K_INTERNET_EVENT_CLASS: u32 = 0x4755524c;
    /// The event ID for getting a URL (kAEGetURL = 'GURL').
    pub const K_AE_GET_URL: u32 = 0x4755524c;
    /// The keyword for the direct object parameter (keyDirectObject = '----').
    pub const KEY_DIRECT_OBJECT: u32 = 0x2d2d2d2d;
}

/// Global sender for URL events.
static URL_SENDER: OnceLock<Sender<String>> = OnceLock::new();

/// Handler for macOS custom URL schemes.
///
/// This type manages the Apple Event handler registration and provides
/// a way to receive URL events.
pub struct UrlHandler {
    receiver: Receiver<String>,
}

impl UrlHandler {
    /// Install the URL handler and return a receiver for URL events.
    ///
    /// This should be called once, early in your application lifecycle,
    /// ideally before the main event loop starts.
    ///
    /// # Returns
    ///
    /// A `UrlHandler` that can be polled for incoming URL events.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    #[must_use]
    pub fn install() -> Self {
        let (sender, receiver) = mpsc::channel();

        // Store sender globally so the Objective-C callback can access it
        URL_SENDER
            .set(sender)
            .expect("UrlHandler::install called more than once");

        // Register the Apple Event handler
        // SAFETY: We're on macOS and this is called from the main thread
        #[allow(unsafe_code)]
        unsafe {
            register_url_handler();
        }

        UrlHandler { receiver }
    }

    /// Try to receive a URL without blocking.
    ///
    /// Returns `Some(url)` if a URL was received, `None` otherwise.
    pub fn try_recv(&self) -> Option<String> {
        self.receiver.try_recv().ok()
    }

    /// Get a reference to the internal receiver for custom polling.
    pub fn receiver(&self) -> &Receiver<String> {
        &self.receiver
    }

    /// Consume the handler and return the receiver.
    pub fn into_receiver(self) -> Receiver<String> {
        self.receiver
    }
}

use objc2::define_class;

define_class!(
    // SAFETY: UrlEventHandler only accesses thread-safe static data and
    // NSObject has no subclassing requirements
    #[unsafe(super(NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[name = "IcyUiUrlEventHandler"]

    /// Objective-C class that handles Apple Events for URLs.
    struct UrlEventHandler;

    impl UrlEventHandler {
        /// Handle incoming URL events from the system.
        #[unsafe(method(handleGetURLEvent:withReplyEvent:))]
        #[allow(non_snake_case)]
        fn handleGetURLEvent_withReplyEvent(
            &self,
            event: *mut AnyObject,
            _reply_event: *mut AnyObject,
        ) {
            forward_urls(event);
        }

        /// Handle documents opened from Finder or Launch Services.
        #[unsafe(method(handleOpenDocumentsEvent:withReplyEvent:))]
        #[allow(non_snake_case)]
        fn handleOpenDocumentsEvent_withReplyEvent(
            &self,
            event: *mut AnyObject,
            _reply_event: *mut AnyObject,
        ) {
            forward_urls(event);
        }
    }

    // SAFETY: NSObjectProtocol methods are inherited from NSObject
    unsafe impl NSObjectProtocol for UrlEventHandler {}
);

impl UrlEventHandler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // Initialize with empty ivars and call super's init
        let this = mtm.alloc::<Self>().set_ivars(());
        // SAFETY: Calling inherited init method from NSObject
        #[allow(unsafe_code)]
        unsafe {
            objc2::msg_send![super(this), init]
        }
    }
}

/// Register the URL event handler with NSAppleEventManager.
///
/// # Safety
///
/// Must be called from the main thread on macOS.
#[allow(unsafe_code)]
unsafe fn register_url_handler() {
    use constants::*;

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("UrlHandler: Not on main thread, cannot register handler");
        return;
    };

    let handler = UrlEventHandler::new(mtm);

    // Get the shared NSAppleEventManager
    let event_manager_class = class!(NSAppleEventManager);
    let shared_manager: *mut AnyObject = msg_send![event_manager_class, sharedAppleEventManager];

    if shared_manager.is_null() {
        log::warn!("UrlHandler: Could not get shared NSAppleEventManager");
        return;
    }

    // Register our handler for kAEGetURL events
    // This is equivalent to:
    // [sharedManager setEventHandler:handler
    //                    andSelector:@selector(handleGetURLEvent:withReplyEvent:)
    //                  forEventClass:kInternetEventClass
    //                     andEventID:kAEGetURL]
    let _: () = msg_send![
        shared_manager,
        setEventHandler: &*handler,
        andSelector: sel!(handleGetURLEvent:withReplyEvent:),
        forEventClass: K_INTERNET_EVENT_CLASS,
        andEventID: K_AE_GET_URL
    ];

    // Register for files opened through Finder, `open`, or Launch Services.
    let _: () = msg_send![
        shared_manager,
        setEventHandler: &*handler,
        andSelector: sel!(handleOpenDocumentsEvent:withReplyEvent:),
        forEventClass: K_CORE_EVENT_CLASS,
        andEventID: K_AE_OPEN_DOCUMENTS
    ];

    // Keep the handler alive by leaking it (it needs to live for the app's lifetime)
    std::mem::forget(handler);

    log::debug!("UrlHandler: Registered Apple Event handlers for URLs and documents");
}

fn forward_urls(event: *mut AnyObject) {
    let Some(sender) = URL_SENDER.get() else {
        return;
    };

    for url in parse_urls_from_event(event) {
        // Ignore send errors (receiver might have been dropped).
        let _ = sender.send(url);
    }
}

/// Parse URLs from a URL or open-documents Apple Event.
#[allow(unsafe_code)]
fn parse_urls_from_event(event: *mut AnyObject) -> Vec<String> {
    use constants::*;

    if event.is_null() {
        return Vec::new();
    }

    unsafe {
        // Verify this is an event handled by this module.
        let event_class: u32 = msg_send![event, eventClass];
        let event_id: u32 = msg_send![event, eventID];

        let is_url = event_class == K_INTERNET_EVENT_CLASS && event_id == K_AE_GET_URL;
        let is_open_documents = event_class == K_CORE_EVENT_CLASS && event_id == K_AE_OPEN_DOCUMENTS;
        if !is_url && !is_open_documents {
            return Vec::new();
        }

        // Get the URL parameter from the event
        // [event paramDescriptorForKeyword:keyDirectObject]
        let subevent: *mut AnyObject =
            msg_send![event, paramDescriptorForKeyword: KEY_DIRECT_OBJECT];

        if subevent.is_null() {
            return Vec::new();
        }

        if is_url {
            return descriptor_string(subevent).into_iter().collect();
        }

        // The direct object of kAEOpenDocuments is an AEList containing file
        // URL descriptors. Apple Event descriptor list indices are one-based.
        let count: isize = msg_send![subevent, numberOfItems];
        let mut urls = Vec::with_capacity(count.max(0) as usize);
        for index in 1..=count {
            let descriptor: *mut AnyObject = msg_send![subevent, descriptorAtIndex: index];
            if let Some(url) = descriptor_file_url(descriptor) {
                urls.push(url);
            }
        }
        urls
    }
}

#[allow(unsafe_code)]
unsafe fn descriptor_file_url(descriptor: *mut AnyObject) -> Option<String> {
    if descriptor.is_null() {
        return None;
    }

    let file_url: *mut AnyObject = unsafe { msg_send![descriptor, fileURLValue] };
    if file_url.is_null() {
        return descriptor_string(descriptor);
    }

    let absolute_string: *mut AnyObject = unsafe { msg_send![file_url, absoluteString] };
    nsstring_to_string(absolute_string)
}

#[allow(unsafe_code)]
fn descriptor_string(descriptor: *mut AnyObject) -> Option<String> {
    if descriptor.is_null() {
        return None;
    }

    let nsstring: *mut AnyObject = unsafe { msg_send![descriptor, stringValue] };
    nsstring_to_string(nsstring)
}

#[allow(unsafe_code)]
fn nsstring_to_string(nsstring: *mut AnyObject) -> Option<String> {
    if nsstring.is_null() {
        return None;
    }

    let cstr: *const std::ffi::c_char = unsafe { msg_send![nsstring, UTF8String] };
    if cstr.is_null() {
        return None;
    }

    Some(unsafe { std::ffi::CStr::from_ptr(cstr) }.to_string_lossy().into_owned())
}
