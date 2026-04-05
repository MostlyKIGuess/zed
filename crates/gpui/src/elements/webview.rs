//! WebView element for rendering HTML content using platform-native webviews.
//!
//! This element is gated behind the `webview` feature flag.
//!
//! On macOS, Windows, and Linux/X11, webviews are embedded as child views
//! inside the GPUI window using wry's `build_as_child()`.
//!
//! On Linux/Wayland, webviews open in separate GTK windows on a dedicated
//! thread because wry's `build_as_child()` does not support Wayland.

use crate::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, StyleRefinement, Styled, Window, px,
};
use refineable::Refineable;
use std::collections::HashMap;
use std::sync::Mutex;

#[cfg(feature = "webview")]
static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

#[cfg(feature = "webview")]
static ACTIVE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// wry::WebView contains raw pointers (not Send/Sync), so we use thread-local
// storage. All GPUI paint calls happen on the main thread.
#[cfg(feature = "webview")]
thread_local! {
    static CHILD_WEBVIEWS: std::cell::RefCell<HashMap<usize, wry::WebView>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(all(
    feature = "webview",
    feature = "gtk",
    any(target_os = "linux", target_os = "freebsd")
))]
use gtk::glib;
#[cfg(all(
    feature = "webview",
    feature = "gtk",
    any(target_os = "linux", target_os = "freebsd")
))]
use gtk::prelude::*;
#[cfg(all(
    feature = "webview",
    feature = "gtk",
    any(target_os = "linux", target_os = "freebsd")
))]
use wry::WebViewBuilderExtUnix;

#[cfg(all(
    feature = "webview",
    feature = "gtk",
    any(target_os = "linux", target_os = "freebsd")
))]
mod gtk_thread {
    use super::*;

    enum GtkMessage {
        Create { id: usize, content: WebViewContent },
        Close { id: usize },
    }

    static CREATED: std::sync::LazyLock<Mutex<std::collections::HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

    // Populated by delete_event so the GPUI render loop can remove stale entries.
    static CLOSED_BY_USER: std::sync::LazyLock<Mutex<Vec<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

    // gtk::Window isn't Send, so we store them on the GTK thread.
    thread_local! {
        static WINDOWS: std::cell::RefCell<HashMap<usize, gtk::Window>> =
            std::cell::RefCell::new(HashMap::new());
    }

    static GTK_SENDER: std::sync::LazyLock<Mutex<std::sync::mpsc::Sender<GtkMessage>>> =
        std::sync::LazyLock::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<GtkMessage>();

            std::thread::spawn(move || {
                if let Err(error) = gtk::init() {
                    log::error!("Failed to initialize GTK: {}", error);
                    return;
                }

                let receiver = std::cell::RefCell::new(receiver);
                glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                    while let Ok(message) = receiver.borrow().try_recv() {
                        match message {
                            GtkMessage::Create { id, content } => create_window(id, content),
                            GtkMessage::Close { id } => close_window(id),
                        }
                    }
                    glib::ControlFlow::Continue
                });

                gtk::main();
            });

            Mutex::new(sender)
        });

    fn create_window(id: usize, content: WebViewContent) {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Zed WebView");
        window.set_default_size(900, 700);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        window.add(&container);
        window.show_all();

        let builder = match &content {
            WebViewContent::Html(html) => wry::WebViewBuilder::new().with_html(html),
            WebViewContent::Url(url) => wry::WebViewBuilder::new().with_url(url),
        };

        match builder.build_gtk(&container) {
            Ok(webview) => {
                log::info!("WebView #{} created (Wayland/GTK)", id);
                ACTIVE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // wry::WebView is dropped when it goes out of scope, which destroys
                // the underlying WebKitGTK widget. We need it alive as long as the
                // GTK window exists, but have no shared owner yet.
                std::mem::forget(webview);
            }
            Err(error) => {
                log::error!("WebView #{}: failed: {}", id, error);
                window.close();
                return;
            }
        }

        let webview_id = id;
        window.connect_delete_event(move |_, _| {
            let was_tracked =
                WINDOWS.with(|windows| windows.borrow_mut().remove(&webview_id).is_some());
            if was_tracked {
                ACTIVE_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                CLOSED_BY_USER
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(webview_id);
            }
            glib::Propagation::Proceed
        });

        WINDOWS.with(|windows| {
            windows.borrow_mut().insert(id, window);
        });
    }

    fn close_window(id: usize) {
        WINDOWS.with(|windows| {
            if let Some(window) = windows.borrow_mut().remove(&id) {
                ACTIVE_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                window.close();
            }
        });
    }

    pub fn drain_closed() -> Vec<usize> {
        let mut closed = CLOSED_BY_USER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::mem::take(&mut *closed)
    }

    pub fn remove_created(id: usize) {
        CREATED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        let sender = GTK_SENDER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = sender.send(GtkMessage::Close { id }) {
            log::error!(
                "WebView #{}: failed to send close to GTK thread: {}",
                id,
                error
            );
        }
    }

    pub fn ensure_created(id: usize, content: &WebViewContent) {
        let mut created = CREATED.lock().unwrap_or_else(|poison| poison.into_inner());
        if created.contains(&id) {
            return;
        }
        created.insert(id);
        drop(created);

        let sender = GTK_SENDER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = sender.send(GtkMessage::Create {
            id,
            content: content.clone(),
        }) {
            log::error!("WebView #{}: failed to send to GTK thread: {}", id, error);
        }
    }
}

#[cfg(feature = "webview")]
fn is_wayland() -> bool {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        crate::guess_compositor() == "Wayland"
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        false
    }
}

#[cfg(feature = "webview")]
fn to_wry_bounds(bounds: Bounds<Pixels>) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
            bounds.origin.x.0 as f64,
            bounds.origin.y.0 as f64,
        )),
        size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
            bounds.size.width.0 as f64,
            bounds.size.height.0 as f64,
        )),
    }
}

#[cfg(feature = "webview")]
fn ensure_gtk_init_for_x11() {
    #[cfg(all(feature = "gtk", any(target_os = "linux", target_os = "freebsd")))]
    {
        // wry's build_as_child on X11 internally creates GTK widgets via
        // gdk_x11_window_foreign_new_for_display, so GTK must be initialized.
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            if let Err(error) = gtk::init() {
                log::error!("Failed to initialize GTK for X11 webview: {}", error);
            }
        });
    }
}

#[cfg(feature = "webview")]
fn pump_gtk_events_for_x11() {
    #[cfg(all(feature = "gtk", any(target_os = "linux", target_os = "freebsd")))]
    {
        while gtk::events_pending() {
            gtk::main_iteration();
        }
    }
}

#[cfg(feature = "webview")]
fn create_child_webview(
    id: usize,
    content: &WebViewContent,
    bounds: Bounds<Pixels>,
    window: &Window,
) {
    ensure_gtk_init_for_x11();

    CHILD_WEBVIEWS.with(|children| {
        if children.borrow().contains_key(&id) {
            return;
        }

        let builder = match content {
            WebViewContent::Html(html) => wry::WebViewBuilder::new().with_html(html),
            WebViewContent::Url(url) => wry::WebViewBuilder::new().with_url(url),
        };

        match builder
            .with_bounds(to_wry_bounds(bounds))
            .with_transparent(true)
            .build_as_child(window)
        {
            Ok(webview) => {
                log::info!("WebView #{} created (child)", id);
                ACTIVE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                children.borrow_mut().insert(id, webview);
            }
            Err(error) => {
                log::error!("WebView #{}: build_as_child failed: {}", id, error);
            }
        }
    });
}

#[cfg(feature = "webview")]
fn update_child_bounds(id: usize, bounds: Bounds<Pixels>) {
    CHILD_WEBVIEWS.with(|children| {
        if let Some(webview) = children.borrow().get(&id) {
            if let Err(error) = webview.set_bounds(to_wry_bounds(bounds)) {
                log::error!("WebView #{}: set_bounds failed: {}", id, error);
            }
        }
    });
}

/// A WebView element for rendering HTML content.
///
/// Each webview is identified by a unique ID. The native webview is created
/// once and persists across GPUI render cycles.
pub struct WebView {
    id: usize,
    content: WebViewContent,
    style: StyleRefinement,
}

/// Content to display in a webview.
#[derive(Clone)]
pub enum WebViewContent {
    /// Raw HTML string.
    Html(String),
    /// URL to navigate to.
    Url(String),
}

impl WebView {
    /// Create a WebView element that renders the given HTML string.
    pub fn from_html(id: usize, html: impl Into<String>) -> Self {
        Self {
            id,
            content: WebViewContent::Html(html.into()),
            style: Default::default(),
        }
    }

    /// Create a WebView element that navigates to the given URL.
    pub fn from_url(id: usize, url: impl Into<String>) -> Self {
        Self {
            id,
            content: WebViewContent::Url(url.into()),
            style: Default::default(),
        }
    }

    /// Allocate a new unique webview ID.
    pub fn next_id() -> usize {
        #[cfg(feature = "webview")]
        {
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
        #[cfg(not(feature = "webview"))]
        {
            0
        }
    }

    /// Return the number of currently open webview windows.
    pub fn active_count() -> usize {
        #[cfg(feature = "webview")]
        {
            ACTIVE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
        }
        #[cfg(not(feature = "webview"))]
        {
            0
        }
    }

    /// Returns IDs of webviews that were closed externally (e.g. by the window manager).
    /// Call this during render to clean up stale entries from your model.
    pub fn drain_closed() -> Vec<usize> {
        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            gtk_thread::drain_closed()
        }
        #[cfg(not(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        )))]
        {
            Vec::new()
        }
    }

    /// Destroy a webview by ID, freeing its resources.
    pub fn remove(id: usize) {
        #[cfg(feature = "webview")]
        {
            CHILD_WEBVIEWS.with(|children| {
                if children.borrow_mut().remove(&id).is_some() {
                    ACTIVE_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            });

            #[cfg(all(feature = "gtk", any(target_os = "linux", target_os = "freebsd")))]
            {
                gtk_thread::remove_created(id);
            }
        }

        #[cfg(not(feature = "webview"))]
        drop(id);
    }

    /// Open a webview window with HTML content immediately.
    /// On Wayland this opens a separate GTK window.
    /// On macOS/Windows/X11 this is a no-op (use the element API instead).
    pub fn open_html(html: impl Into<String>) {
        let html = html.into();

        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gtk_thread::ensure_created(id, &WebViewContent::Html(html));
        }

        #[cfg(not(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        )))]
        drop(html);
    }

    /// Open a webview window navigating to a URL immediately.
    pub fn open_url(url: impl Into<String>) {
        let url = url.into();

        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gtk_thread::ensure_created(id, &WebViewContent::Url(url));
        }

        #[cfg(not(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        )))]
        drop(url);
    }
}

impl Element for WebView {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        if matches!(style.size.width, crate::geometry::Length::Auto) {
            style.size.width = crate::geometry::Length::Definite(px(400.0).into());
        }
        if matches!(style.size.height, crate::geometry::Length::Auto) {
            style.size.height = crate::geometry::Length::Definite(px(300.0).into());
        }
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        #[cfg(feature = "webview")]
        {
            if is_wayland() {
                // Wayland: separate GTK window (build_as_child not supported)
                #[cfg(all(feature = "gtk", any(target_os = "linux", target_os = "freebsd")))]
                gtk_thread::ensure_created(self.id, &self.content);
            } else {
                // macOS, Windows, X11: embed as child of the GPUI window
                let exists =
                    CHILD_WEBVIEWS.with(|children| children.borrow().contains_key(&self.id));
                if exists {
                    update_child_bounds(self.id, bounds);
                } else {
                    create_child_webview(self.id, &self.content, bounds, window);
                }
                pump_gtk_events_for_x11();
            }
        }
    }
}

impl IntoElement for WebView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for WebView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webview_from_html() {
        let webview = WebView::from_html(0, "<html><body>Hello</body></html>");
        assert!(matches!(webview.content, WebViewContent::Html(_)));
    }

    #[test]
    fn test_webview_from_url() {
        let webview = WebView::from_url(0, "https://example.com");
        assert!(matches!(webview.content, WebViewContent::Url(_)));
    }
}
