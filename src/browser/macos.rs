//! Native Chromium child views. CEF owns rendering, input and Retina scaling.
use super::{Event, runtime};
use cef::*;
use gpui::{Bounds, Pixels, Window as GpuiWindow};
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::{Retained, Weak as ObjcWeak, autoreleasepool},
    runtime::AnyObject,
};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSEvent, NSEventMask, NSEventModifierFlags, NSView, NSWindow,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type Tx = async_channel::Sender<Event>;
fn page_url(browser: Option<&Browser>) -> String {
    browser
        .and_then(|b| b.main_frame())
        .map(|f| CefString::from(&f.url()).to_string())
        .unwrap_or_default()
}
fn web_url(url: &str) -> bool {
    url == "about:blank"
        || url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}
wrap_display_handler! {
    struct Display { tx: Tx }
    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title { let _ = self.tx.try_send(Event::Title(title.to_string())); }
        }
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() != 0) {
                if let Some(url) = url { let _ = self.tx.try_send(Event::Loading(url.to_string())); }
            }
        }
    }
}
wrap_load_handler! {
    struct Load { tx: Tx }
    impl LoadHandler {
        fn on_loading_state_change(&self, browser: Option<&mut Browser>, loading: i32, _back: i32, _forward: i32) {
            let url = page_url(browser.as_deref());
            let _ = self.tx.try_send(if loading != 0 { Event::Loading(url) } else { Event::Loaded(url) });
        }
        fn on_load_error(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, code: Errorcode, text: Option<&CefString>, _url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() != 0) && code != Errorcode::ABORTED {
                let _ = self.tx.try_send(Event::Error(text.map(CefString::to_string).unwrap_or_else(|| "Page failed to load".into())));
            }
        }
    }
}
wrap_request_handler! {
    struct Navigation;
    impl RequestHandler {
        fn on_before_browse(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, request: Option<&mut Request>, _gesture: i32, _redirect: i32) -> i32 {
            if !frame.is_some_and(|f| f.is_main() != 0) { return 0; }
            request.is_some_and(|r| !web_url(&CefString::from(&r.url()).to_string())) as i32
        }
    }
}
wrap_life_span_handler! {
    struct Life { tx: Tx }
    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) { runtime::ACTIVE.fetch_add(1, Ordering::Relaxed); }
        fn on_before_close(&self, _browser: Option<&mut Browser>) { runtime::ACTIVE.fetch_sub(1, Ordering::Relaxed); }
        fn do_close(&self, browser: Option<&mut Browser>) -> i32 {
            // Tear down this child view, never Herdr's top-level window.
            if let Some(host) = browser.and_then(|b| b.host()) {
                if let Some(view) = unsafe { host.window_handle().cast::<NSView>().as_ref() } { view.removeFromSuperview(); }
            }
            1
        }
        fn on_before_popup(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, _id: i32, url: Option<&CefString>, _name: Option<&CefString>, _disposition: WindowOpenDisposition, _gesture: i32, _features: Option<&PopupFeatures>, _window: Option<&mut WindowInfo>, _client: Option<&mut Option<Client>>, _settings: Option<&mut BrowserSettings>, _extra: Option<&mut Option<DictionaryValue>>, _no_js: Option<&mut i32>) -> i32 {
            if let Some(url) = url { if web_url(&url.to_string()) { let _ = self.tx.try_send(Event::Open(url.to_string())); } }
            1
        }
    }
}
wrap_focus_handler! {
    struct Focus { gate: Arc<AtomicBool> }
    impl FocusHandler {
        fn on_set_focus(&self, _browser: Option<&mut Browser>, _source: FocusSource) -> i32 { (!self.gate.load(Ordering::Relaxed)) as i32 }
    }
}
wrap_download_handler! {
    struct Download { tx: Tx }
    impl DownloadHandler {
        fn can_download(&self, _browser: Option<&mut Browser>, url: Option<&CefString>, _method: Option<&CefString>) -> i32 {
            url.is_some_and(|url| web_url(&url.to_string()) && url.to_string() != "about:blank") as i32
        }
        fn on_before_download(&self, _browser: Option<&mut Browser>, _item: Option<&mut DownloadItem>, name: Option<&CefString>, callback: Option<&mut BeforeDownloadCallback>) -> i32 {
            let folder = crate::mods::app_dir().join("downloads");
            if std::fs::create_dir_all(&folder).is_err() { let _ = self.tx.try_send(Event::Notice("Cannot create downloads folder".into())); return 1; }
            let suggested = name.map(CefString::to_string).unwrap_or_else(|| "download".into());
            let name = std::path::Path::new(&suggested).file_name().unwrap_or_default();
            let mut path = folder.join(name); let mut i = 1;
            while path.exists() { path = folder.join(format!("{i}-{}", name.to_string_lossy())); i += 1; }
            if let Some(callback) = callback {
                callback.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
                let _ = self.tx.try_send(Event::Notice(format!("Downloading to {}", path.display())));
            }
            1
        }
        fn on_download_updated(&self, _browser: Option<&mut Browser>, item: Option<&mut DownloadItem>, _callback: Option<&mut DownloadItemCallback>) {
            if let Some(item) = item {
                if item.is_complete() != 0 { let _ = self.tx.try_send(Event::Notice(format!("Download saved to {}", CefString::from(&item.full_path())))); }
                else if item.is_canceled() != 0 { let _ = self.tx.try_send(Event::Notice("Download canceled".into())); }
            }
        }
    }
}
wrap_client! {
    struct PageClient { tx: Tx, gate: Arc<AtomicBool> }
    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> { Some(Display::new(self.tx.clone())) }
        fn load_handler(&self) -> Option<LoadHandler> { Some(Load::new(self.tx.clone())) }
        fn request_handler(&self) -> Option<RequestHandler> { Some(Navigation::new()) }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> { Some(Life::new(self.tx.clone())) }
        fn focus_handler(&self) -> Option<FocusHandler> { Some(Focus::new(self.gate.clone())) }
        fn download_handler(&self) -> Option<DownloadHandler> { Some(Download::new(self.tx.clone())) }
    }
}
pub struct Host {
    browser: runtime::BrowserCell,
    container: Retained<NSView>,
    parent: Retained<NSView>,
    window: Retained<NSWindow>,
    shown: Cell<bool>,
    allowed: Rc<Cell<bool>>,
    bounds: Cell<Option<[f64; 4]>>,
    focus_pending: Cell<bool>,
    focus_gate: Arc<AtomicBool>,
    monitor: Option<Retained<AnyObject>>,
}
fn focused(container: &NSView, window: &NSWindow) -> bool {
    let mut responder = window.firstResponder();
    for _ in 0..64 {
        let Some(current) = responder else {
            return false;
        };
        if let Some(view) = current.downcast_ref::<NSView>() {
            if view.isDescendantOf(container) {
                return true;
            }
        }
        responder = unsafe { current.nextResponder() };
    }
    false
}
fn shortcut(key: &str, shift: bool) -> Option<&'static str> {
    match (key, shift) {
        ("[" | "{", true) => Some("tab.prev"),
        ("]" | "}", true) => Some("tab.next"),
        ("[", false) => Some("browser.back"),
        ("]", false) => Some("browser.forward"),
        ("l", false) => Some("browser.focus_url"),
        ("r", false) => Some("browser.reload"),
        ("t", false) => Some("browser.open"),
        ("w", false) => Some("tab.close"),
        ("p", true) => Some("app.palette"),
        (",", false) => Some("app.settings"),
        ("i", true) => Some("browser.devtools"),
        _ => None,
    }
}
impl Host {
    pub fn new(
        url: &str,
        allowed: Rc<Cell<bool>>,
        tx: Tx,
        window: &GpuiWindow,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            runtime::ready(),
            "Chromium runtime is unavailable; launch the packaged app"
        );
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| anyhow::anyhow!("Browser creation must run on the UI thread"))?;
        let RawWindowHandle::AppKit(handle) = HasWindowHandle::window_handle(window)?.as_raw()
        else {
            anyhow::bail!("Expected an AppKit window")
        };
        // GPUI owns this view for the window lifetime; retain our references.
        let parent = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
            .ok_or_else(|| anyhow::anyhow!("Missing parent view"))?;
        let native_window = parent
            .window()
            .ok_or_else(|| anyhow::anyhow!("Missing native window"))?;
        if std::env::var_os("HERDR_BROWSER_TEST_WINDOW_FRONT").is_some() {
            native_window.orderFrontRegardless();
        }
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0., 0.), NSSize::new(1., 1.)),
        );
        container.setHidden(true);
        parent.addSubview(&container);
        let info = WindowInfo {
            runtime_style: RuntimeStyle::CHROME,
            ..Default::default()
        }
        .set_as_child(
            Retained::as_ptr(&container).cast_mut().cast(),
            &Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        );
        let focus_gate = Arc::new(AtomicBool::new(false));
        let mut client = PageClient::new(tx.clone(), focus_gate.clone());
        let browser = browser_host_create_browser_sync(
            Some(&info),
            Some(&mut client),
            Some(&url.into()),
            Some(&BrowserSettings::default()),
            None,
            None,
        )
        .ok_or_else(|| anyhow::anyhow!("Chromium could not create a native browser"))?;
        let browser = Rc::new(RefCell::new(Some(browser)));
        let weak_container = ObjcWeak::from_retained(&container);
        let weak_parent = ObjcWeak::from_retained(&parent);
        runtime::register(&browser, move || {
            autoreleasepool(|_| {
                if let Some(container) = weak_container.load() {
                    if let (Some(window), Some(parent)) = (container.window(), weak_parent.load()) {
                        window.makeFirstResponder(Some(&parent));
                    }
                    container.setHidden(true);
                    container.removeFromSuperview();
                    // Drop the temporary array before the pool drains, releasing
                    // CEF's host view and delivering its WindowDestroyed callback.
                    let children = container.subviews();
                    for child in children.iter() {
                        child.removeFromSuperview();
                    }
                }
            })
        });
        let weak = Rc::downgrade(&browser);
        let event_allowed = allowed.clone();
        let event_container = container.clone();
        let event_window = native_window.clone();
        let block = block2::RcBlock::new(move |event: NonNull<NSEvent>| {
            let e = unsafe { event.as_ref() };
            if !event_allowed.get() || e.windowNumber() != event_window.windowNumber() {
                return event.as_ptr();
            }
            if e.r#type() == objc2_app_kit::NSEventType::LeftMouseDown {
                let p = event_container.convertPoint_fromView(e.locationInWindow(), None);
                let b = event_container.bounds();
                if p.x >= 0. && p.y >= 0. && p.x < b.size.width && p.y < b.size.height {
                    let _ = tx.try_send(Event::Focused);
                }
                return event.as_ptr();
            }
            if !focused(&event_container, &event_window) {
                return event.as_ptr();
            }
            let flags = e.modifierFlags();
            if !flags.contains(NSEventModifierFlags::Command)
                || flags.intersects(NSEventModifierFlags::Option | NSEventModifierFlags::Control)
            {
                return event.as_ptr();
            }
            let key = e
                .charactersIgnoringModifiers()
                .map(|s| s.to_string().to_lowercase())
                .unwrap_or_default();
            let shift = flags.contains(NSEventModifierFlags::Shift);
            // App navigation remains available before a page has a focused frame.
            if let Some(command) = shortcut(&key, shift) {
                let _ = tx.try_send(if command == "browser.focus_url" {
                    Event::FocusUrl
                } else {
                    Event::Command(command)
                });
                return std::ptr::null_mut();
            }
            if let Some(cell) = weak.upgrade() {
                if let Some(frame) = cell.borrow().as_ref().and_then(|b| b.focused_frame()) {
                    match (key.as_str(), shift) {
                        ("c", false) => frame.copy(),
                        ("v", false) => frame.paste(),
                        ("x", false) => frame.cut(),
                        ("a", false) => frame.select_all(),
                        ("z", false) => frame.undo(),
                        ("z", true) => frame.redo(),
                        _ => return event.as_ptr(),
                    }
                    return std::ptr::null_mut();
                }
            }
            event.as_ptr()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown | NSEventMask::LeftMouseDown,
                &block,
            )
        };
        Ok(Self {
            browser,
            container,
            parent,
            window: native_window,
            shown: Cell::new(false),
            allowed,
            bounds: Cell::new(None),
            focus_pending: Cell::new(true),
            focus_gate,
            monitor,
        })
    }
    fn native_view(&self) -> Option<&NSView> {
        let browser = self.browser.borrow();
        let ptr = browser.as_ref()?.host()?.window_handle().cast::<NSView>();
        // CEF owns this view until close(); callers stay on the UI thread.
        unsafe { ptr.as_ref() }
    }
    pub fn set_allowed(&self, allowed: bool) {
        self.allowed.set(allowed);
        if !allowed {
            self.focus_gate.store(false, Ordering::Relaxed);
        }
        if !allowed && self.shown.replace(false) {
            if self.is_native_focused() {
                self.focus_pending.set(true);
                self.focus_parent();
            }
            self.container.setHidden(true);
        }
    }
    pub fn place(&self, bounds: Bounds<Pixels>, allowed: bool) {
        let rect = [
            f64::from(f32::from(bounds.origin.x)),
            f64::from(f32::from(bounds.origin.y)),
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        ];
        if !allowed || rect[2] < 1. || rect[3] < 1. {
            self.set_allowed(false);
            return;
        }
        let y = if self.parent.isFlipped() {
            rect[1]
        } else {
            self.parent.bounds().size.height - rect[1] - rect[3]
        };
        self.container.setFrame(NSRect::new(
            NSPoint::new(rect[0], y),
            NSSize::new(rect[2], rect[3]),
        ));
        if let Some(view) = self.native_view() {
            view.setFrame(self.container.bounds());
            view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
        }
        self.bounds.set(Some(rect));
        self.focus_gate.store(true, Ordering::Relaxed);
        if !self.shown.replace(true) {
            self.container.setHidden(false);
        }
        if self.focus_pending.replace(false) {
            self.focus();
        }
    }
    fn is_native_focused(&self) -> bool {
        focused(&self.container, &self.window)
    }
    pub fn is_focused(&self) -> bool {
        self.shown.get() && self.is_native_focused()
    }
    pub fn focus(&self) {
        if self.shown.get() {
            if let Some(host) = self.browser.borrow().as_ref().and_then(|b| b.host()) {
                host.set_focus(1);
            }
        } else {
            self.focus_pending.set(true);
        }
    }
    pub fn focus_parent(&self) {
        self.window.makeFirstResponder(Some(&self.parent));
    }
    pub fn navigate(&self, url: &str) -> anyhow::Result<()> {
        let frame = self
            .browser
            .borrow()
            .as_ref()
            .and_then(|b| b.main_frame())
            .ok_or_else(|| anyhow::anyhow!("Browser is closed"))?;
        frame.load_url(Some(&url.into()));
        Ok(())
    }
    pub fn command(&self, command: &str) -> Result<(), String> {
        let browser = self.browser.borrow();
        let browser = browser.as_ref().ok_or("Browser is closed")?;
        match command {
            "browser.back" => browser.go_back(),
            "browser.forward" => browser.go_forward(),
            "browser.reload" => browser.reload(),
            "browser.devtools" => {
                if let Some(host) = browser.host() {
                    host.show_dev_tools(
                        Some(&WindowInfo::default()),
                        None,
                        Some(&BrowserSettings::default()),
                        None,
                    );
                }
            }
            _ => return Err(format!("Unknown browser command: {command}")),
        }
        Ok(())
    }
    pub fn inspect(&self) -> Value {
        let browser = self.browser.borrow();
        let browser = browser.as_ref();
        json!({"url":page_url(browser), "bounds":self.bounds.get(), "shown":self.shown.get(), "focused":self.is_focused(), "window":self.window.windowNumber(), "scale_factor":self.window.backingScaleFactor(), "window_visible":self.window.isVisible(), "window_key":self.window.isKeyWindow(), "parent_hidden":self.parent.isHidden(), "parent_bounds":[self.parent.bounds().size.width,self.parent.bounds().size.height], "can_go_back":browser.is_some_and(|b| b.can_go_back() != 0), "can_go_forward":browser.is_some_and(|b| b.can_go_forward() != 0), "active_browsers":runtime::ACTIVE.load(Ordering::Relaxed), "pump_ticks":runtime::TICKS.load(Ordering::Relaxed)})
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            unsafe {
                NSEvent::removeMonitor(&monitor);
            }
        }
        self.container.setHidden(true);
        runtime::close(&self.browser);
        self.container.removeFromSuperview();
    }
}
#[cfg(test)]
mod tests {
    use super::shortcut;
    #[test]
    fn shifted_brackets_always_switch_herdr_tabs() {
        assert_eq!(shortcut("[", true), Some("tab.prev"));
        assert_eq!(shortcut("{", true), Some("tab.prev"));
        assert_eq!(shortcut("]", true), Some("tab.next"));
        assert_eq!(shortcut("}", true), Some("tab.next"));
        assert_eq!(shortcut("[", false), Some("browser.back"));
    }
}
