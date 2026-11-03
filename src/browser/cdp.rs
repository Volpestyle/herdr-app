//! Playwright access to agent-profile pages over Chromium's DevTools pipe.
//!
//! Chromium never opens a network debugging port. Herdr owns the pipe (fds 3/4)
//! and serves a guarded WebSocket on 127.0.0.1 whose path carries a random
//! per-launch token, handed out only by `browser.cdp` over the user-only control
//! socket. Each client gets its own browser-target session; the router hides
//! every target outside the agent profile, denies browser-level methods that
//! could reach person sessions or close the app, and pins profile-scoped
//! methods to the agent profile.
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Herdr's ends: (to Chromium, from Chromium). Closing them quits Chromium.
static PIPE: OnceLock<Mutex<Option<(File, File)>>> = OnceLock::new();
static BRIDGE: OnceLock<Result<Arc<Bridge>, String>> = OnceLock::new();
const MAX_CLIENTS: usize = 8;

fn fd_open(fd: i32) -> bool {
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Chromium reads commands on fd 3 and writes replies on fd 4. Claim both before
/// anything else in the process can open a file there.
pub fn reserve() {
    if std::env::var("HERDR_BROWSER_CDP").is_ok_and(|v| v == "0") {
        return;
    }
    if fd_open(3) || fd_open(4) {
        eprintln!("herdr-app: fd 3/4 already open; Playwright access stays off");
        return;
    }
    let mut to = [0; 2];
    let mut from = [0; 2];
    unsafe {
        if libc::pipe(to.as_mut_ptr()) != 0 || libc::pipe(from.as_mut_ptr()) != 0 {
            return;
        }
        // pipe() may itself return 3 or 4; lift every end above them first.
        let lift = |fd: i32| {
            let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
            libc::close(fd);
            high
        };
        let (to_read, to_write, from_read, from_write) =
            (lift(to[0]), lift(to[1]), lift(from[0]), lift(from[1]));
        if libc::dup2(to_read, 3) != 3 || libc::dup2(from_write, 4) != 4 {
            return;
        }
        libc::close(to_read);
        libc::close(from_write);
        let _ = PIPE.set(Mutex::new(Some((
            File::from_raw_fd(to_write),
            File::from_raw_fd(from_read),
        ))));
    }
}

pub fn reserved() -> bool {
    PIPE.get().is_some()
}

/// Start (once) and return the guarded endpoint for this agent profile.
/// `pane` is the agent pane asking; Playwright's new pages open beside it.
pub fn endpoint(agent_context: &str, pane: Option<&str>) -> Result<String, String> {
    let bridge = BRIDGE
        .get_or_init(Bridge::start)
        .clone()?;
    bridge.router.lock().map_err(|_| "CDP bridge failed")?.set_agent(agent_context)?;
    if let (Some(pane), Ok(mut origin)) = (pane, bridge.origin.lock()) {
        origin.get_or_insert_with(|| pane.to_string());
    }
    Ok(format!("ws://127.0.0.1:{}/devtools/browser/{}", bridge.port, bridge.token))
}

struct Bridge {
    port: u16,
    token: String,
    to: Mutex<File>,
    router: Mutex<Router>,
    clients: Mutex<HashMap<u64, mpsc::Sender<Option<String>>>>,
    origin: Mutex<Option<String>>,
}

impl Bridge {
    fn start() -> Result<Arc<Self>, String> {
        let (to, from) = PIPE
            .get()
            .and_then(|p| p.lock().ok()?.take())
            .ok_or("Playwright access is unavailable: Chromium's DevTools pipe was not reserved")?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let bridge = Arc::new(Self {
            port,
            token: token()?,
            to: Mutex::new(to),
            router: Mutex::new(Router::default()),
            clients: Mutex::new(HashMap::new()),
            origin: Mutex::new(None),
        });
        let reader = bridge.clone();
        std::thread::Builder::new()
            .name("herdr-cdp-pipe".into())
            .spawn(move || reader.read_pipe(from))
            .map_err(|e| e.to_string())?;
        let acceptor = bridge.clone();
        std::thread::Builder::new()
            .name("herdr-cdp-listen".into())
            .spawn(move || {
                let mut next = 0u64;
                for stream in listener.incoming().flatten() {
                    next += 1;
                    let (client, id) = (acceptor.clone(), next);
                    let _ = std::thread::Builder::new()
                        .name("herdr-cdp-client".into())
                        .spawn(move || client.serve(id, stream));
                }
            })
            .map_err(|e| e.to_string())?;
        // The bridge lives for the process; the pipe must never close early.
        Ok(bridge)
    }

    fn dispatch(self: &Arc<Self>, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Browser(message) => {
                    let mut bytes = message.to_string().into_bytes();
                    bytes.push(0);
                    if let Ok(mut to) = self.to.lock() {
                        let _ = to.write_all(&bytes);
                    }
                }
                Out::Client(id, message) => {
                    if let Some(tx) = self.clients.lock().ok().and_then(|c| c.get(&id).cloned()) {
                        let _ = tx.send(Some(message.to_string()));
                    }
                }
                Out::Open { client, id, url } => {
                    let bridge = self.clone();
                    let _ = std::thread::Builder::new().name("herdr-cdp-open".into()).spawn(move || {
                        let outs = match bridge.open_page(&url) {
                            Ok(page) => bridge.router.lock().map(|mut r| r.created(client, id, page)).unwrap_or_default(),
                            Err(e) => vec![Out::Client(client, error(&id, &e))],
                        };
                        bridge.dispatch(outs);
                    });
                }
                Out::Close(id) => {
                    if let Some(tx) = self.clients.lock().ok().and_then(|c| c.get(&id).cloned()) {
                        let _ = tx.send(None);
                    }
                }
            }
        }
    }

    /// Playwright's new page is a real agent pane the owner can see, never a
    /// hidden target: open it through the app's own command path.
    fn open_page(&self, url: &str) -> Result<Value, String> {
        let origin = self.origin.lock().ok().and_then(|o| o.clone());
        let beside = origin.map(|pane| json!({"profile": "agent", "url": url, "place": "right", "pane": pane}));
        let opened = match beside.map(|args| crate::control::request("browser.open", args)) {
            Some(Ok(opened)) => opened,
            // The minting pane may have closed; a new tab is still visible.
            _ => crate::control::request("browser.open", json!({"profile": "agent", "url": url, "place": "tab"}))?,
        };
        let pane = opened["pane"].as_str().ok_or("Herdr did not name the new pane")?.to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        // A new CEF view starts as an "other" target; Playwright only adopts pages.
        loop {
            match crate::control::request("browser.cdp", json!({"pane": pane})) {
                Ok(page) if page["type"] == "page" && page["targetId"].is_string() => return Ok(page),
                Ok(_) if std::time::Instant::now() >= deadline => return Err("The new pane never became a page".into()),
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn read_pipe(self: Arc<Self>, mut from: File) {
        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let n = match from.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buffer.extend_from_slice(&chunk[..n]);
            while let Some(end) = buffer.iter().position(|b| *b == 0) {
                let raw: Vec<u8> = buffer.drain(..=end).collect();
                let Ok(message) = serde_json::from_slice::<Value>(&raw[..raw.len() - 1]) else {
                    continue;
                };
                let outs = match self.router.lock() {
                    Ok(mut router) => router.from_browser(message),
                    Err(_) => return,
                };
                self.dispatch(outs);
            }
        }
        tracing::warn!("Chromium closed its DevTools pipe");
        // Keep `from` alive until here; dropping it earlier would quit Chromium.
        drop(from);
    }

    fn serve(self: Arc<Self>, id: u64, stream: TcpStream) {
        let (port, token) = (self.port, self.token.clone());
        let check = move |request: &tungstenite::handshake::server::Request,
                          response: tungstenite::handshake::server::Response| {
            let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
            match authorize(request.uri().path(), header("host"), header("origin"), &token, port) {
                Ok(()) => Ok(response),
                Err(reason) => {
                    let mut refusal = tungstenite::handshake::server::ErrorResponse::new(Some(reason.into()));
                    *refusal.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
                    Err(refusal)
                }
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Ok(mut socket) = tungstenite::accept_hdr(stream, check) else { return };
        let (tx, rx) = mpsc::channel();
        {
            let Ok(mut clients) = self.clients.lock() else { return };
            if clients.len() >= MAX_CLIENTS {
                let _ = socket.close(None);
                return;
            }
            clients.insert(id, tx);
        }
        let outs = self.router.lock().map(|mut r| r.connect(id)).unwrap_or_default();
        self.dispatch(outs);
        let _ = socket.get_mut().set_read_timeout(Some(Duration::from_millis(5)));
        'serve: loop {
            match socket.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let outs = match (serde_json::from_str::<Value>(&text), self.router.lock()) {
                        (Ok(message), Ok(mut router)) => router.from_client(id, message),
                        _ => break,
                    };
                    self.dispatch(outs);
                }
                Ok(tungstenite::Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
            while let Ok(next) = rx.try_recv() {
                match next {
                    Some(text) => {
                        if socket.send(tungstenite::Message::text(text)).is_err() {
                            break 'serve;
                        }
                    }
                    None => {
                        let _ = socket.close(None);
                        let _ = socket.flush();
                        break 'serve;
                    }
                }
            }
        }
        if let Ok(mut clients) = self.clients.lock() {
            clients.remove(&id);
        }
        let outs = self.router.lock().map(|mut r| r.disconnect(id)).unwrap_or_default();
        self.dispatch(outs);
    }
}

fn token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("Cannot create a CDP token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The token gate. Browsers always send Origin on WebSocket upgrades, so any
/// Origin is refused (a web page can never connect); Host must be loopback on
/// our port (no DNS rebinding); the path must carry the per-launch token.
pub fn authorize(path: &str, host: Option<&str>, origin: Option<&str>, token: &str, port: u16) -> Result<(), &'static str> {
    if origin.is_some() {
        return Err("browser origins may not connect");
    }
    let host = host.ok_or("missing Host")?;
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return Err("Host must be this loopback endpoint");
    }
    let given = path.strip_prefix("/devtools/browser/").ok_or("unknown path")?;
    let equal = given.len() == token.len()
        && given.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if !equal {
        return Err("invalid token");
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum Out {
    Browser(Value),
    Client(u64, Value),
    /// Open a visible agent pane for Target.createTarget, then answer `id`.
    Open { client: u64, id: Value, url: String },
    Close(u64),
}

#[derive(Debug)]
enum Pending {
    Root(u64),
    /// `root`: the client sent it on its browser session, so the reply carries no sessionId.
    Client { client: u64, id: Value, filter: Filter, root: bool },
    /// Our own attach to a pane Playwright asked for; answers createTarget `id`.
    Created { client: u64, id: Value, info: Value },
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Filter {
    None,
    Targets,
    Attach,
}

#[derive(Default, Debug)]
struct Client {
    root: Option<String>,
    sessions: HashSet<String>,
    queued: Vec<Value>,
}

/// Pure CDP routing and policy; the bridge does all I/O.
#[derive(Default, Debug)]
pub struct Router {
    agent: Option<String>,
    next: u64,
    pending: HashMap<u64, Pending>,
    clients: HashMap<u64, Client>,
    owner: HashMap<String, u64>,
    allowed: HashSet<String>,
    downloads: HashSet<String>,
    /// Agent page sessions per client, for profile-scoped calls CDP cannot
    /// address by context id (CEF profiles are not DevTools browser contexts).
    pages: HashMap<String, u64>,
    /// The one session each client was told about per page, so each page is
    /// announced once and only that session's detach reaches Playwright
    /// (which keys pages by target id).
    announced: HashMap<(u64, String), String>,
}

/// Root (browser-session) methods that pass through unchanged.
const ROOT_PASS: &[&str] = &["Browser.getVersion", "Target.setAutoAttach", "Target.setDiscoverTargets"];
/// Root methods addressed to one target, which must be an agent-profile target.
const ROOT_TARGET: &[&str] = &[
    "Target.attachToTarget",
    "Target.activateTarget",
    "Target.closeTarget",
    "Target.getTargetInfo",
    "Browser.getWindowForTarget",
];
/// Playwright's cookie calls, translated to page-scoped Network calls on one of
/// the client's agent pages. Storage.* without a resolvable context id reads
/// the default (person) profile even from a page session, so it never passes.
/// clearCookies has no page-scoped equivalent that cannot reach the person.
fn cookie_call(method: &str) -> Option<&'static str> {
    match method {
        "Storage.getCookies" => Some("Network.getAllCookies"),
        "Storage.setCookies" => Some("Network.setCookies"),
        _ => None,
    }
}
/// Never allowed from any session: they reach other profiles, other sessions or quit.
const DENIED: &[&str] = &[
    "Target.attachToBrowserTarget",
    "Target.exposeDevToolsProtocol",
    "Target.createBrowserContext",
    "Target.disposeBrowserContext",
    "Target.getBrowserContexts",
    "Target.setRemoteLocations",
    "Target.sendMessageToTarget",
    "Target.attachToTarget",
    "Target.createTarget",
    "Target.getTargets",
    "Target.setDiscoverTargets",
    "Target.activateTarget",
    "Target.closeTarget",
    "Browser.close",
    "Browser.crash",
    "Browser.crashGpuProcess",
    "Browser.executeBrowserCommand",
    // Default-context storage (the person profile) whatever session sends it.
    "Storage.getCookies",
    "Storage.setCookies",
    "Storage.clearCookies",
    "Storage.clearDataForOrigin",
    "Storage.clearDataForStorageKey",
    "Network.clearBrowserCookies",
    "Network.clearBrowserCache",
];

fn error(id: &Value, message: &str) -> Value {
    json!({"id": id, "error": {"code": -32000, "message": message}})
}

impl Router {
    pub fn set_agent(&mut self, context: &str) -> Result<(), String> {
        match &self.agent {
            Some(known) if known != context => Err("The agent profile changed; restart Herdr Browser".into()),
            _ => {
                self.agent = Some(context.to_string());
                Ok(())
            }
        }
    }

    fn id(&mut self, pending: Pending) -> u64 {
        self.next += 1;
        self.pending.insert(self.next, pending);
        self.next
    }

    fn agent_target(&self, info: &Value) -> bool {
        self.agent.as_deref().is_some_and(|a| info["browserContextId"].as_str() == Some(a))
            && info["type"].as_str() != Some("browser")
    }

    pub fn connect(&mut self, client: u64) -> Vec<Out> {
        self.clients.insert(client, Client::default());
        let id = self.id(Pending::Root(client));
        vec![Out::Browser(json!({"id": id, "method": "Target.attachToBrowserTarget"}))]
    }

    pub fn disconnect(&mut self, client: u64) -> Vec<Out> {
        let Some(state) = self.clients.remove(&client) else { return vec![] };
        self.owner.retain(|_, c| *c != client);
        self.pages.retain(|_, c| *c != client);
        self.announced.retain(|(c, _), _| *c != client);

        match state.root {
            Some(root) => {
                let id = self.id(Pending::Internal);
                vec![Out::Browser(json!({"id": id, "method": "Target.detachFromTarget", "params": {"sessionId": root}}))]
            }
            None => vec![],
        }
    }

    pub fn from_client(&mut self, client: u64, message: Value) -> Vec<Out> {
        let Some(state) = self.clients.get_mut(&client) else { return vec![] };
        let Some(root) = state.root.clone() else {
            state.queued.push(message);
            return vec![];
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default().to_string();
        let mut params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let deny = |why: &str| vec![Out::Client(client, error(&id, why))];
        if !id.is_u64() || method.is_empty() {
            return deny("Malformed CDP request");
        }
        if let Some(context) = params.get("browserContextId").and_then(Value::as_str) {
            if self.agent.as_deref() != Some(context) {
                return deny("Herdr exposes only the agent profile over CDP");
            }
        }
        let session = message.get("sessionId").and_then(Value::as_str).map(str::to_string);
        let mut filter = Filter::None;
        let target_session = match session {
            Some(session) => {
                if !state.sessions.contains(&session) {
                    return deny("Unknown CDP session");
                }
                if DENIED.contains(&method.as_str()) {
                    return deny(&format!("Herdr blocks {method} on page sessions"));
                }
                if method == "Target.detachFromTarget" && !self.owns_detach(client, &params) {
                    return deny("Unknown CDP session");
                }
                session
            }
            None => {
                let method = method.as_str();
                if method == "Target.getBrowserContexts" {
                    return vec![Out::Client(client, json!({"id": id, "result": {"browserContextIds": []}}))];
                }
                if method == "Browser.close" {
                    // Disconnect this client; never close the owner's browser.
                    return vec![Out::Client(client, json!({"id": id, "result": {}})), Out::Close(client)];
                }
                if method == "Target.getTargets" {
                    filter = Filter::Targets;
                } else if method == "Target.getTargetInfo" && params.get("targetId").is_none() {
                    // The browser target's own info: no page, no profile.
                } else if method == "Target.attachToTarget" || ROOT_TARGET.contains(&method) {
                    let target = params["targetId"].as_str().unwrap_or_default();
                    if !self.allowed.contains(target) {
                        return deny("Herdr exposes only agent-profile targets over CDP");
                    }
                    if method == "Target.attachToTarget" {
                        if params["flatten"] != true {
                            return deny("Herdr requires flattened CDP sessions");
                        }
                        filter = Filter::Attach;
                    }
                } else if method == "Target.detachFromTarget" {
                    if !self.owns_detach(client, &params) {
                        return deny("Unknown CDP session");
                    }
                } else if method == "Target.createTarget" {
                    let url = params["url"].as_str().filter(|u| !u.is_empty()).unwrap_or("about:blank");
                    return vec![Out::Open { client, id, url: url.to_string() }];
                } else if matches!(method, "Browser.setDownloadBehavior" | "Browser.cancelDownload") {
                    // Herdr's own download handler saves agent downloads in the app home.
                    return vec![Out::Client(client, json!({"id": id, "result": {}}))];
                } else if let Some(network) = cookie_call(method) {
                    let Some(page) = self.pages.iter().find(|(_, c)| **c == client).map(|(s, _)| s.clone()) else {
                        return deny("Attach an agent page before using cookies");
                    };
                    let params = match network {
                        "Network.setCookies" => json!({"cookies": params["cookies"]}),
                        _ => json!({}),
                    };
                    let global = self.id(Pending::Client { client, id, filter, root: true });
                    return vec![Out::Browser(json!({"id": global, "method": network, "params": params, "sessionId": page}))];
                } else if !ROOT_PASS.contains(&method) {
                    return deny(&format!("Herdr blocks {method} over CDP"));
                }
                root.clone()
            }
        };
        let global = self.id(Pending::Client { client, id, filter, root: target_session == root });
        vec![Out::Browser(json!({"id": global, "method": method, "params": params, "sessionId": target_session}))]
    }

    fn owns_detach(&self, client: u64, params: &Value) -> bool {
        match (params["sessionId"].as_str(), params["targetId"].as_str()) {
            (Some(session), _) => self.owner.get(session) == Some(&client),
            (None, Some(target)) => self.allowed.contains(target),
            _ => false,
        }
    }

    pub fn from_browser(&mut self, mut message: Value) -> Vec<Out> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            return match self.pending.remove(&id) {
                Some(Pending::Root(client)) => self.rooted(client, &message),
                Some(Pending::Client { client, id, filter, root }) => {
                    if !self.clients.contains_key(&client) {
                        return vec![];
                    }
                    message["id"] = id;
                    if let (true, Some(map)) = (root, message.as_object_mut()) {
                        map.remove("sessionId");
                    }
                    match filter {
                        Filter::Targets => {
                            let infos = message["result"]["targetInfos"].as_array().cloned().unwrap_or_default();
                            let kept: Vec<Value> = infos.into_iter().filter(|i| self.agent_target(i)).collect();
                            for info in &kept {
                                if let Some(t) = info["targetId"].as_str() {
                                    self.allowed.insert(t.into());
                                }
                            }
                            message["result"]["targetInfos"] = Value::Array(kept);
                        }
                        Filter::Attach => {
                            if let Some(session) = message["result"]["sessionId"].as_str() {
                                self.adopt(client, session);
                            }
                        }
                        Filter::None => {}
                    }
                    vec![Out::Client(client, message)]
                }
                Some(Pending::Created { client, id, info }) => {
                    let Some(session) = message["result"]["sessionId"].as_str().map(str::to_string) else {
                        let why = message["error"]["message"].as_str().unwrap_or("Cannot attach to the new pane").to_string();
                        return vec![Out::Client(client, error(&id, &why))];
                    };
                    let target = info["targetId"].clone();
                    let reply = Out::Client(client, json!({"id": id, "result": {"targetId": target}}));
                    let key = (client, target.as_str().unwrap_or_default().to_string());
                    if self.owner.get(&session) == Some(&client) {
                        // Chromium already announced this attach itself.
                        return vec![reply];
                    }
                    if self.announced.contains_key(&key) {
                        // Its own auto-attach won the race; drop our duplicate session.
                        let root = self.clients.get(&client).and_then(|c| c.root.clone());
                        let detach = self.id(Pending::Internal);
                        return vec![Out::Browser(json!({"id": detach, "method": "Target.detachFromTarget", "params": {"sessionId": session}, "sessionId": root})), reply];
                    }
                    self.announced.insert(key, session.clone());
                    self.adopt(client, &session);
                    self.pages.insert(session.clone(), client);
                    vec![
                        Out::Client(client, json!({"method": "Target.attachedToTarget", "params": {"sessionId": session, "targetInfo": info, "waitingForDebugger": false}})),
                        reply,
                    ]
                }
                Some(Pending::Internal) | None => vec![],
            };
        }
        let Some(session) = message.get("sessionId").and_then(Value::as_str).map(str::to_string) else {
            return vec![];
        };
        let Some(&client) = self.owner.get(&session) else { return vec![] };
        let root = self.clients.get(&client).and_then(|c| c.root.clone());
        let method = message["method"].as_str().unwrap_or_default().to_string();
        if root.as_deref() != Some(session.as_str()) {
            // A page or child session this client owns.
            match method.as_str() {
                "Target.attachedToTarget" => {
                    if let Some(child) = message["params"]["sessionId"].as_str() {
                        self.adopt(client, child);
                    }
                }
                "Target.detachedFromTarget" => {
                    if let Some(child) = message["params"]["sessionId"].as_str() {
                        self.owner.remove(child);
                    }
                }
                _ => {}
            }
            return vec![Out::Client(client, message)];
        }
        if let Some(map) = message.as_object_mut() {
            map.remove("sessionId");
        }
        let params = &message["params"];
        let forward = match method.as_str() {
            "Target.attachedToTarget" => {
                let child = params["sessionId"].as_str().unwrap_or_default().to_string();
                let target = params["targetInfo"]["targetId"].as_str().unwrap_or_default().to_string();
                let page = params["targetInfo"]["type"] == "page";
                let fresh = !page || !self.announced.contains_key(&(client, target.clone()));
                if fresh && self.agent_target(&params["targetInfo"]) {
                    if page {
                        self.announced.insert((client, target.clone()), child.clone());
                    }
                    if let Some(t) = params["targetInfo"]["targetId"].as_str() {
                        self.allowed.insert(t.into());
                    }
                    if params["targetInfo"]["type"] == "page" {
                        self.pages.insert(child.clone(), client);
                    }
                    self.adopt(client, &child);
                    true
                } else {
                    // Hidden: release a paused target and let go of it.
                    let mut outs = vec![];
                    if params["waitingForDebugger"] == true {
                        let id = self.id(Pending::Internal);
                        outs.push(Out::Browser(json!({"id": id, "method": "Runtime.runIfWaitingForDebugger", "sessionId": child})));
                    }
                    let id = self.id(Pending::Internal);
                    outs.push(Out::Browser(json!({"id": id, "method": "Target.detachFromTarget", "params": {"sessionId": child}, "sessionId": session})));
                    return outs;
                }
            }
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let ok = self.agent_target(&params["targetInfo"]);
                if let (true, Some(t)) = (ok, params["targetInfo"]["targetId"].as_str()) {
                    self.allowed.insert(t.into());
                }
                ok
            }
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                let target = params["targetId"].as_str().unwrap_or_default().to_string();
                let ok = self.allowed.contains(&target);
                if method == "Target.targetDestroyed" {
                    self.allowed.remove(&target);
                    self.announced.retain(|(_, t), _| *t != target);
                }
                ok
            }
            "Target.detachedFromTarget" => {
                let child = params["sessionId"].as_str().unwrap_or_default().to_string();
                let key = (client, params["targetId"].as_str().unwrap_or_default().to_string());
                self.pages.remove(&child);
                let owned = self.owner.remove(&child).is_some();
                match self.announced.get(&key) {
                    // A superseded session of an announced page: Playwright would close the page.
                    Some(current) if *current != child => false,
                    Some(_) => {
                        self.announced.remove(&key);
                        owned
                    }
                    None => owned,
                }
            }
            "Browser.downloadWillBegin" => {
                let ok = self.allowed.contains(params["frameId"].as_str().unwrap_or_default());
                if let (true, Some(guid)) = (ok, params["guid"].as_str()) {
                    self.downloads.insert(guid.into());
                }
                ok
            }
            "Browser.downloadProgress" => self.downloads.contains(params["guid"].as_str().unwrap_or_default()),
            _ => false,
        };
        if forward { vec![Out::Client(client, message)] } else { vec![] }
    }

    /// The new pane is a page now. Its auto-attach happened while it was an
    /// "other" target, which Playwright ignores, so attach to it again and
    /// announce it before createTarget's reply, as Chromium orders them.
    pub fn created(&mut self, client: u64, id: Value, page: Value) -> Vec<Out> {
        let (Some(target), Some(agent)) = (page["targetId"].as_str(), self.agent.clone()) else {
            return vec![Out::Client(client, error(&id, "No target for the new pane"))];
        };
        let Some(root) = self.clients.get(&client).and_then(|c| c.root.clone()) else { return vec![] };
        self.allowed.insert(target.to_string());
        if self.announced.contains_key(&(client, target.to_string())) {
            return vec![Out::Client(client, json!({"id": id, "result": {"targetId": target}}))];
        }
        let info = json!({"targetId": target, "type": "page", "title": "", "url": page["url"], "attached": true,
                          "canAccessOpener": false, "browserContextId": agent});
        let global = self.id(Pending::Created { client, id, info });
        vec![Out::Browser(json!({"id": global, "method": "Target.attachToTarget", "params": {"targetId": target, "flatten": true}, "sessionId": root}))]
    }

    fn adopt(&mut self, client: u64, session: &str) {
        self.owner.insert(session.to_string(), client);
        if let Some(state) = self.clients.get_mut(&client) {
            state.sessions.insert(session.to_string());
        }
    }

    fn rooted(&mut self, client: u64, message: &Value) -> Vec<Out> {
        let Some(session) = message["result"]["sessionId"].as_str() else {
            return vec![Out::Close(client)];
        };
        self.owner.insert(session.to_string(), client);
        let Some(state) = self.clients.get_mut(&client) else { return vec![] };
        state.root = Some(session.to_string());
        let queued = std::mem::take(&mut state.queued);
        queued.into_iter().flat_map(|m| self.from_client(client, m)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "a1b2c3d4";

    #[test]
    fn the_token_gate_admits_only_a_loopback_client_with_the_token() {
        assert_eq!(authorize("/devtools/browser/a1b2c3d4", Some("127.0.0.1:9333"), None, TOKEN, 9333), Ok(()));
        assert_eq!(authorize("/devtools/browser/a1b2c3d4", Some("localhost:9333"), None, TOKEN, 9333), Ok(()));
        assert_eq!(authorize("/devtools/browser/a1b2c3d5", Some("127.0.0.1:9333"), None, TOKEN, 9333), Err("invalid token"));
        assert_eq!(authorize("/devtools/browser/a1b2c3d", Some("127.0.0.1:9333"), None, TOKEN, 9333), Err("invalid token"));
        assert_eq!(authorize("/devtools/browser/", Some("127.0.0.1:9333"), None, TOKEN, 9333), Err("invalid token"));
        assert_eq!(authorize("/json/version", Some("127.0.0.1:9333"), None, TOKEN, 9333), Err("unknown path"));
        // A web page (any Origin) and DNS rebinding (foreign Host) are refused even with the token.
        assert_eq!(authorize("/devtools/browser/a1b2c3d4", Some("127.0.0.1:9333"), Some("https://evil.example"), TOKEN, 9333), Err("browser origins may not connect"));
        assert_eq!(authorize("/devtools/browser/a1b2c3d4", Some("evil.example:9333"), None, TOKEN, 9333), Err("Host must be this loopback endpoint"));
        assert_eq!(authorize("/devtools/browser/a1b2c3d4", None, None, TOKEN, 9333), Err("missing Host"));
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (a, b) = (token().unwrap(), token().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    fn rooted() -> Router {
        let mut r = Router::default();
        r.set_agent("AGENT").unwrap();
        let outs = r.connect(1);
        let Out::Browser(m) = &outs[0] else { panic!() };
        assert_eq!(m["method"], "Target.attachToBrowserTarget");
        let id = m["id"].clone();
        assert!(r.from_browser(json!({"id": id, "result": {"sessionId": "ROOT"}})).is_empty());
        r
    }

    fn sent(outs: &[Out]) -> &Value {
        match &outs[0] {
            Out::Browser(m) => m,
            other => panic!("expected a browser message, got {other:?}"),
        }
    }

    fn denied(outs: &[Out]) -> bool {
        matches!(&outs[..], [Out::Client(1, m)] if m.get("error").is_some())
    }

    #[test]
    fn person_profile_targets_are_hidden_and_released() {
        let mut r = rooted();
        let person = r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "P1", "waitingForDebugger": true,
            "targetInfo": {"targetId": "TP", "type": "page", "browserContextId": "PERSON"}}}));
        assert_eq!(person.len(), 2);
        assert_eq!(sent(&person)["method"], "Runtime.runIfWaitingForDebugger");
        assert!(matches!(&person[1], Out::Browser(m) if m["method"] == "Target.detachFromTarget"));
        // Its session can never be used, and its target never addressed.
        assert!(denied(&r.from_client(1, json!({"id": 1, "method": "Runtime.evaluate", "sessionId": "P1"}))));
        assert!(denied(&r.from_client(1, json!({"id": 2, "method": "Target.attachToTarget", "params": {"targetId": "TP", "flatten": true}}))));
        assert!(r.from_browser(json!({"method": "Target.targetCreated", "sessionId": "ROOT", "params": {"targetInfo": {"targetId": "TP", "type": "page", "browserContextId": "PERSON"}}})).is_empty());

        let agent = r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "A1", "waitingForDebugger": true,
            "targetInfo": {"targetId": "TA", "type": "page", "browserContextId": "AGENT"}}}));
        assert!(matches!(&agent[..], [Out::Client(1, m)] if m["params"]["sessionId"] == "A1" && m.get("sessionId").is_none()));
        let page = r.from_client(1, json!({"id": 3, "method": "Runtime.evaluate", "sessionId": "A1", "params": {"expression": "1"}}));
        assert_eq!(sent(&page)["sessionId"], "A1");
        let reply = r.from_browser(json!({"id": sent(&page)["id"], "sessionId": "A1", "result": {}}));
        assert_eq!(reply, vec![Out::Client(1, json!({"id": 3, "sessionId": "A1", "result": {}}))]);
    }

    #[test]
    fn target_lists_show_only_the_agent_profile() {
        let mut r = rooted();
        let outs = r.from_client(1, json!({"id": 7, "method": "Target.getTargets"}));
        let global = sent(&outs)["id"].clone();
        assert_eq!(sent(&outs)["sessionId"], "ROOT");
        let reply = r.from_browser(json!({"id": global, "sessionId": "ROOT", "result": {"targetInfos": [
            {"targetId": "TP", "type": "page", "browserContextId": "PERSON"},
            {"targetId": "TA", "type": "page", "browserContextId": "AGENT"},
            {"targetId": "TB", "type": "browser", "browserContextId": "AGENT"}]}}));
        let Out::Client(1, m) = &reply[0] else { panic!() };
        assert_eq!(m["id"], 7);
        assert_eq!(m["result"]["targetInfos"].as_array().unwrap().len(), 1);
        assert_eq!(m["result"]["targetInfos"][0]["targetId"], "TA");
        let contexts = r.from_client(1, json!({"id": 8, "method": "Target.getBrowserContexts"}));
        assert!(matches!(&contexts[..], [Out::Client(1, m)] if m["result"]["browserContextIds"] == json!([])));
    }

    #[test]
    fn profile_methods_stay_in_the_agent_profile() {
        let mut r = rooted();
        // New pages become visible Herdr agent panes, never hidden targets.
        let create = r.from_client(1, json!({"id": 1, "method": "Target.createTarget", "params": {"url": "", "newWindow": true, "background": true}}));
        assert_eq!(create, vec![Out::Open { client: 1, id: json!(1), url: "about:blank".into() }]);
        // Once the pane is a page, the bridge attaches and announces it before the reply.
        let attach = r.created(1, json!(1), json!({"targetId": "TN", "type": "page", "url": "about:blank"}));
        assert_eq!(sent(&attach)["method"], "Target.attachToTarget");
        assert_eq!(sent(&attach)["sessionId"], "ROOT");
        let done = r.from_browser(json!({"id": sent(&attach)["id"], "sessionId": "ROOT", "result": {"sessionId": "N1"}}));
        assert!(matches!(&done[0], Out::Client(1, m) if m["method"] == "Target.attachedToTarget" && m["params"]["targetInfo"]["type"] == "page"));
        assert_eq!(done[1], Out::Client(1, json!({"id": 1, "result": {"targetId": "TN"}})));
        let use_it = r.from_client(1, json!({"id": 2, "method": "Page.enable", "sessionId": "N1"}));
        assert_eq!(sent(&use_it)["sessionId"], "N1");
        // Chromium announcing our explicit attach itself is not a duplicate.
        let again = r.created(1, json!(4), json!({"targetId": "TQ", "type": "page"}));
        let global = sent(&again)["id"].clone();
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "Q1", "targetInfo": {"targetId": "TQ", "type": "page", "browserContextId": "AGENT"}}}));
        let reply = r.from_browser(json!({"id": global, "sessionId": "ROOT", "result": {"sessionId": "Q1"}}));
        assert_eq!(reply, vec![Out::Client(1, json!({"id": 4, "result": {"targetId": "TQ"}}))]);
        // Detaching a superseded "other" session of TN must not close the page in Playwright.
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "O1", "targetInfo": {"targetId": "TN", "type": "other", "browserContextId": "AGENT"}}}));
        let stale = r.from_browser(json!({"method": "Target.detachedFromTarget", "sessionId": "ROOT", "params": {"sessionId": "O1", "targetId": "TN"}}));
        assert!(stale.is_empty(), "{stale:?}");
        // A late auto-attach of the same page is hidden, never announced twice.
        let late = r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "N2", "targetInfo": {"targetId": "TN", "type": "page", "browserContextId": "AGENT"}}}));
        assert!(late.iter().all(|o| matches!(o, Out::Browser(_))), "{late:?}");
        // And a page already announced by auto-attach gets a direct reply.
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "M1", "targetInfo": {"targetId": "TM", "type": "page", "browserContextId": "AGENT"}}}));
        assert_eq!(r.created(1, json!(3), json!({"targetId": "TM", "type": "page"})), vec![Out::Client(1, json!({"id": 3, "result": {"targetId": "TM"}}))]);
        assert!(denied(&r.from_client(1, json!({"id": 4, "method": "Target.createTarget", "params": {"url": "x", "browserContextId": "PERSON"}}))));
        // Cookies need an agent page session and run there, scoped to its profile.
        assert!(denied(&rooted().from_client(1, json!({"id": 2, "method": "Storage.getCookies"}))));
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "A1", "targetInfo": {"targetId": "TA", "type": "page", "browserContextId": "AGENT"}}}));
        let cookies = r.from_client(1, json!({"id": 2, "method": "Storage.getCookies", "params": {"browserContextId": "AGENT"}}));
        // Any of this client's agent pages; all share the agent profile.
        assert!(["N1", "M1", "A1"].contains(&sent(&cookies)["sessionId"].as_str().unwrap()), "{cookies:?}");
        assert_eq!(sent(&cookies)["method"], "Network.getAllCookies");
        assert_eq!(sent(&cookies)["params"], json!({}));
        let set = r.from_client(1, json!({"id": 7, "method": "Storage.setCookies", "params": {"cookies": [{"name": "a", "value": "b"}], "browserContextId": "AGENT"}}));
        assert_eq!(sent(&set)["method"], "Network.setCookies");
        assert_eq!(sent(&set)["params"], json!({"cookies": [{"name": "a", "value": "b"}]}));
        assert!(denied(&r.from_client(1, json!({"id": 8, "method": "Storage.clearCookies"}))));
        // Page sessions cannot reach default-context storage either.
        assert!(denied(&r.from_client(1, json!({"id": 9, "method": "Storage.getCookies", "sessionId": "A1"}))));
        assert!(denied(&r.from_client(1, json!({"id": 10, "method": "Network.clearBrowserCookies", "sessionId": "A1"}))));
        assert!(denied(&r.from_client(1, json!({"id": 3, "method": "Storage.getCookies", "params": {"browserContextId": "PERSON"}}))));
        let downloads = r.from_client(1, json!({"id": 5, "method": "Browser.setDownloadBehavior", "params": {"behavior": "allowAndName"}}));
        assert_eq!(downloads, vec![Out::Client(1, json!({"id": 5, "result": {}}))]);
        assert!(denied(&r.from_client(1, json!({"id": 6, "method": "Browser.grantPermissions", "params": {"permissions": ["geolocation"]}}))));
    }

    #[test]
    fn dangerous_methods_are_denied_and_close_only_disconnects() {
        let mut r = rooted();
        for method in ["Target.attachToBrowserTarget", "Target.createBrowserContext", "Target.exposeDevToolsProtocol", "Browser.crash", "SystemInfo.getProcessInfo", "Tethering.bind"] {
            assert!(denied(&r.from_client(1, json!({"id": 1, "method": method}))), "{method}");
        }
        let own = r.from_client(1, json!({"id": 5, "method": "Target.getTargetInfo"}));
        assert_eq!(sent(&own)["method"], "Target.getTargetInfo");
        assert!(denied(&r.from_client(1, json!({"id": 6, "method": "Target.getTargetInfo", "params": {"targetId": "TP"}}))));
        let close = r.from_client(1, json!({"id": 2, "method": "Browser.close"}));
        assert_eq!(close, vec![Out::Client(1, json!({"id": 2, "result": {}})), Out::Close(1)]);
        // Page sessions cannot reach other targets either.
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "A1", "targetInfo": {"targetId": "TA", "type": "page", "browserContextId": "AGENT"}}}));
        assert!(denied(&r.from_client(1, json!({"id": 3, "method": "Target.attachToTarget", "sessionId": "A1", "params": {"targetId": "TP"}}))));
        assert!(denied(&r.from_client(1, json!({"id": 4, "method": "Target.attachToBrowserTarget", "sessionId": "A1"}))));
    }

    #[test]
    fn clients_cannot_use_each_others_sessions() {
        let mut r = rooted();
        let outs = r.connect(2);
        let id = sent(&outs)["id"].clone();
        r.from_browser(json!({"id": id, "result": {"sessionId": "ROOT2"}}));
        r.from_browser(json!({"method": "Target.attachedToTarget", "sessionId": "ROOT", "params": {
            "sessionId": "A1", "targetInfo": {"targetId": "TA", "type": "page", "browserContextId": "AGENT"}}}));
        let other = r.from_client(2, json!({"id": 1, "method": "Runtime.evaluate", "sessionId": "A1"}));
        assert!(matches!(&other[..], [Out::Client(2, m)] if m.get("error").is_some()));
        // Root ids never leak: a client cannot name another client's root session.
        let root = r.from_client(2, json!({"id": 2, "method": "Target.getTargets", "sessionId": "ROOT"}));
        assert!(matches!(&root[..], [Out::Client(2, m)] if m.get("error").is_some()));
    }

    #[test]
    fn requests_before_the_root_session_wait_for_it() {
        let mut r = Router::default();
        r.set_agent("AGENT").unwrap();
        let outs = r.connect(1);
        let id = sent(&outs)["id"].clone();
        assert!(r.from_client(1, json!({"id": 1, "method": "Browser.getVersion"})).is_empty());
        let flushed = r.from_browser(json!({"id": id, "result": {"sessionId": "ROOT"}}));
        assert_eq!(sent(&flushed)["method"], "Browser.getVersion");
        assert_eq!(sent(&flushed)["sessionId"], "ROOT");
    }

    #[test]
    fn the_agent_profile_cannot_change_once_known() {
        let mut r = Router::default();
        assert!(r.set_agent("AGENT").is_ok());
        assert!(r.set_agent("AGENT").is_ok());
        assert!(r.set_agent("OTHER").is_err());
    }
}
