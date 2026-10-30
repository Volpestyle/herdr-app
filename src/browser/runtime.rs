//! CEF's native message pump shares GPUI's main thread; no polling render loop.
use cef::*;
use dispatch2::DispatchQueue;
use objc2::{
    ClassType, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, ProtocolBuilder, Sel},
    sel,
};
use objc2_app_kit::{NSApplication, NSEvent};
use objc2_foundation::{NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSTimer};
use std::ptr::NonNull;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub type BrowserCell = Rc<RefCell<Option<Browser>>>;
type BrowserEntry = (Weak<RefCell<Option<Browser>>>, Box<dyn Fn()>);
thread_local! {
    static READY: Cell<bool> = const { Cell::new(false) };
    static SEND_EVENT: Cell<bool> = const { Cell::new(false) };
    static BROWSERS: RefCell<Vec<BrowserEntry>> = const { RefCell::new(Vec::new()) };
    static PUMP: RefCell<Option<Arc<Pump>>> = const { RefCell::new(None) };
}
pub static ACTIVE: AtomicUsize = AtomicUsize::new(0);
pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn ready() -> bool {
    READY.with(Cell::get)
}
pub fn register(browser: &BrowserCell, detach: impl Fn() + 'static) {
    BROWSERS.with_borrow_mut(|items| items.push((Rc::downgrade(browser), Box::new(detach))));
}

struct Pump {
    generation: AtomicU64,
    alive: AtomicBool,
    working: AtomicBool,
}
impl Pump {
    fn schedule(self: &Arc<Self>, delay: i64) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let pump = self.clone();
        // CEF explicitly replaces previously scheduled work. Always dispatch, even
        // for zero delay, so calls cannot recurse from a CEF callback.
        DispatchQueue::main().exec_async(move || {
            if !pump.alive.load(Ordering::Relaxed)
                || pump.generation.load(Ordering::Relaxed) != generation
            {
                return;
            }
            let work = block2::RcBlock::new(move |_timer: NonNull<NSTimer>| {
                if !pump.alive.load(Ordering::Relaxed)
                    || pump.generation.load(Ordering::Relaxed) != generation
                {
                    return;
                }
                if pump.working.swap(true, Ordering::Relaxed) {
                    pump.schedule(1);
                    return;
                }
                do_message_loop_work();
                TICKS.fetch_add(1, Ordering::Relaxed);
                pump.working.store(false, Ordering::Relaxed);
                if pump.generation.load(Ordering::Relaxed) == generation {
                    pump.schedule(33);
                }
            });
            // Run CEF from AppKit's timer source, outside a main-queue block.
            unsafe {
                let timer = NSTimer::timerWithTimeInterval_repeats_block(
                    (delay.clamp(1, 33) as f64) / 1000.,
                    false,
                    &work,
                );
                NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
            }
        });
    }
}

wrap_browser_process_handler! {
    struct ProcessHandler { pump: Arc<Pump> }
    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) { self.pump.schedule(delay_ms); }
    }
}
wrap_app! {
    struct BrowserApp { pump: Arc<Pump> }
    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> { Some(ProcessHandler::new(self.pump.clone())) }
    }
}

extern "C-unwind" fn sending(_: &AnyObject, _: Sel) -> Bool {
    Bool::new(SEND_EVENT.with(Cell::get))
}
extern "C-unwind" fn set_sending(_: &AnyObject, _: Sel, value: Bool) {
    SEND_EVENT.with(|s| s.set(value.as_bool()));
}
extern "C-unwind" fn send_event(this: &AnyObject, _: Sel, event: &NSEvent) {
    let previous = SEND_EVENT.with(|s| s.replace(true));
    // Our subclass has no event implementation between it and NSApplication.
    unsafe {
        let _: () = msg_send![super(this, NSApplication::class()), sendEvent: event];
    }
    SEND_EVENT.with(|s| s.set(previous));
}
extern "C-unwind" fn terminate(this: &AnyObject, _: Sel, sender: Option<&AnyObject>) {
    stop();
    // GPUI's delegate still performs its normal persistence/quit callbacks.
    unsafe {
        let _: () = msg_send![super(this, NSApplication::class()), terminate: sender];
    }
}

fn application() -> anyhow::Result<()> {
    let superclass = AnyClass::get(c"GPUIApplication")
        .ok_or_else(|| anyhow::anyhow!("GPUI application class is unavailable"))?;
    let mut builder = ClassBuilder::new(c"HerdrChromiumApplication", superclass)
        .ok_or_else(|| anyhow::anyhow!("Chromium application class already exists"))?;
    let mut parent = None;
    for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
        let protocol = AnyProtocol::get(name).unwrap_or_else(|| {
            // These protocols are declared by the CEF client, not necessarily
            // exported by the dynamically loaded Chromium framework.
            let mut protocol = ProtocolBuilder::new(name).expect("Protocol registration");
            if let Some(parent) = parent {
                protocol.add_protocol(parent);
            }
            if name == c"CrAppProtocol" {
                protocol.add_method_description::<(), Bool>(sel!(isHandlingSendEvent), true);
            }
            if name == c"CrAppControlProtocol" {
                protocol.add_method_description::<(Bool,), ()>(sel!(setHandlingSendEvent:), true);
            }
            protocol.register()
        });
        builder.add_protocol(protocol);
        parent = Some(protocol);
    }
    // Chromium requires these selectors around AppKit event delivery. The
    // superclass retains GPUI's platform ivar and application behavior.
    unsafe {
        builder.add_method(
            sel!(isHandlingSendEvent),
            sending as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(setHandlingSendEvent:),
            set_sending as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(sendEvent:),
            send_event as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(sel!(terminate:), terminate as extern "C-unwind" fn(_, _, _));
    }
    let class = builder.register();
    let app: Retained<NSApplication> = unsafe { msg_send![class, sharedApplication] };
    anyhow::ensure!(
        app.isKindOfClass(class),
        "NSApplication was created before Chromium could integrate with GPUI"
    );
    Ok(())
}

/// Called only on an actual GUI launch, after CLI handling and before GPUI runs.
pub fn prepare() -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Missing app bundle"))?;
    let framework = contents.join("Frameworks/Chromium Embedded Framework.framework");
    anyhow::ensure!(
        framework.is_dir(),
        "Chromium is not packaged. Run personal/package_preview.py first."
    );
    let loader = library_loader::LibraryLoader::new(&executable, false);
    anyhow::ensure!(loader.load(), "Cannot load bundled Chromium");
    // Keep code mapped until process exit, including native view deallocation.
    std::mem::forget(loader);
    application()?;
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let contents = executable.parent().unwrap().parent().unwrap();
    let framework = contents.join("Frameworks/Chromium Embedded Framework.framework");
    let pump = Arc::new(Pump {
        generation: AtomicU64::new(0),
        alive: AtomicBool::new(true),
        working: AtomicBool::new(false),
    });
    let mut app = BrowserApp::new(pump.clone());
    let args = args::Args::new();
    let profile = crate::mods::app_dir().join("chromium");
    std::fs::create_dir_all(&profile)?;
    let settings = Settings {
        no_sandbox: 0,
        external_message_pump: 1,
        command_line_args_disabled: 1,
        framework_dir_path: framework.to_string_lossy().as_ref().into(),
        browser_subprocess_path: contents
            .join("Frameworks/HerdrBrowser Helper.app/Contents/MacOS/HerdrBrowser Helper")
            .to_string_lossy()
            .as_ref()
            .into(),
        main_bundle_path: contents.parent().unwrap().to_string_lossy().as_ref().into(),
        root_cache_path: profile.to_string_lossy().as_ref().into(),
        cache_path: profile.join("Default").to_string_lossy().as_ref().into(),
        persist_session_cookies: 1,
        keychain_service_name: std::env::var("HERDR_BROWSER_TEST_KEYCHAIN")
            .unwrap_or_else(|_| "Herdr Browser Personal Safe Storage".into())
            .as_str()
            .into(),
        keychain_account_name: "Herdr Browser".into(),
        log_file: crate::mods::app_dir()
            .join("chromium.log")
            .to_string_lossy()
            .as_ref()
            .into(),
        log_severity: LogSeverity::WARNING,
        remote_debugging_port: std::env::var("HERDR_BROWSER_DEBUG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0),
        ..Default::default()
    };
    anyhow::ensure!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut()
        ) == 1,
        "Chromium initialization failed"
    );
    PUMP.with_borrow_mut(|p| *p = Some(pump));
    READY.with(|r| r.set(true));
    Ok(())
}

pub fn close(cell: &BrowserCell) {
    if let Some(browser) = cell.borrow_mut().take() {
        if ready() && browser.is_valid() != 0 {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
    }
}

pub fn stop() {
    if !ready() {
        return;
    }
    let cells = BROWSERS.with_borrow_mut(|b| std::mem::take(b));
    for (browser, detach) in cells {
        // Native CEF closes when its child view is deallocated. GPUI entities
        // remain alive during application termination, so detach explicitly.
        detach();
        if let Some(cell) = browser.upgrade() {
            close(&cell);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while ACTIVE.load(Ordering::Relaxed) != 0 && Instant::now() < deadline {
        objc2::rc::autoreleasepool(|_| do_message_loop_work());
    }
    PUMP.with_borrow_mut(|p| {
        if let Some(p) = p.take() {
            p.alive.store(false, Ordering::Relaxed);
        }
    });
    READY.with(|r| r.set(false));
    if ACTIVE.load(Ordering::Relaxed) == 0 {
        shutdown();
    } else {
        tracing::warn!("Chromium still has live pages at process exit; skipping unsafe shutdown");
    }
}
