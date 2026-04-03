//! WebView element for rendering HTML content using platform-native webviews.
//!
//! This element is gated behind the `webview` feature flag.
//!
//! On Linux, all webviews run on a single dedicated GTK thread. New webview
//! requests are sent to it via a channel and created within GTK's main loop.

use crate::{
    App, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Style,
    StyleRefinement, Styled, Window, px,
};
use refineable::Refineable;

#[cfg(all(
    feature = "webview",
    feature = "gtk",
    any(target_os = "linux", target_os = "freebsd")
))]
use gtk::glib;
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
    use std::sync::Mutex;

    struct WebViewRequest {
        id: usize,
        content: WebViewContent,
    }

    static CREATED_WEBVIEWS: std::sync::LazyLock<Mutex<std::collections::HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

    static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

    static ACTIVE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    static GTK_SENDER: std::sync::LazyLock<Mutex<std::sync::mpsc::Sender<WebViewRequest>>> =
        std::sync::LazyLock::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<WebViewRequest>();

            std::thread::spawn(move || {
                if let Err(error) = gtk::init() {
                    log::error!("Failed to initialize GTK: {}", error);
                    return;
                }

                let receiver = std::cell::RefCell::new(receiver);
                glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                    while let Ok(request) = receiver.borrow().try_recv() {
                        create_window(request);
                    }
                    glib::ControlFlow::Continue
                });

                gtk::main();
            });

            Mutex::new(sender)
        });

    fn create_window(request: WebViewRequest) {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Zed WebView");
        window.set_default_size(900, 700);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        window.add(&container);
        window.show_all();

        let builder = match &request.content {
            WebViewContent::Html(html) => wry::WebViewBuilder::new().with_html(html),
            WebViewContent::Url(url) => wry::WebViewBuilder::new().with_url(url),
        };

        let webview_id = request.id;
        match builder.build_gtk(&container) {
            Ok(webview) => {
                log::info!("WebView #{} created", webview_id);
                ACTIVE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // wry::WebView is dropped when it goes out of scope, which destroys
                // the underlying WebKitGTK widget. We need it alive as long as the
                // GTK window exists, but have no shared owner yet.
                std::mem::forget(webview);
            }
            Err(error) => {
                log::error!("WebView #{}: failed: {}", webview_id, error);
                window.close();
                return;
            }
        }

        window.connect_delete_event(move |_, _| {
            CREATED_WEBVIEWS
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&webview_id);
            ACTIVE_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            glib::Propagation::Proceed
        });
    }

    pub fn next_id() -> usize {
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn active_count() -> usize {
        ACTIVE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn ensure_created(id: usize, content: &WebViewContent) {
        let mut created = CREATED_WEBVIEWS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if created.contains(&id) {
            return;
        }
        created.insert(id);
        drop(created);

        let sender = GTK_SENDER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = sender.send(WebViewRequest {
            id,
            content: content.clone(),
        }) {
            log::error!("WebView #{}: failed to send to GTK thread: {}", id, error);
        }
    }
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
        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            gtk_thread::next_id()
        }

        #[cfg(not(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        )))]
        {
            0
        }
    }

    /// Return the number of currently open webview windows.
    pub fn active_count() -> usize {
        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            gtk_thread::active_count()
        }

        #[cfg(not(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        )))]
        {
            0
        }
    }

    /// Open a webview window with HTML content immediately.
    /// Can be called from click handlers or any non-render context.
    pub fn open_html(html: impl Into<String>) {
        let html = html.into();

        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            let id = gtk_thread::next_id();
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
            let id = gtk_thread::next_id();
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
        _bounds: crate::Bounds<crate::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: crate::Bounds<crate::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        #[cfg(all(
            feature = "webview",
            feature = "gtk",
            any(target_os = "linux", target_os = "freebsd")
        ))]
        {
            gtk_thread::ensure_created(self.id, &self.content);
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
