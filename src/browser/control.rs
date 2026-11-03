//! Pane-local, in-process CDP. All calls and observer callbacks stay on CEF's UI thread.
//! No debugger socket, native input, or window activation is involved.
use base64::Engine as _;
use cef::*;
use gpui::{BackgroundExecutor, FutureExt};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

static NEXT_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static NEXT_ACK: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1 << 30);

struct Recording {
    dir: std::path::PathBuf,
    t0: f64,
    size: (f64, f64, f64),
    script: Option<String>,
    frames: Vec<(std::path::PathBuf, f64)>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

fn save_frame(recording: &mut Recording, params: &[u8], browser: Option<&mut Browser>) {
    let Ok(frame) = serde_json::from_slice::<Value>(params) else { return };
    if let Some(host) = browser.and_then(|b| b.host()) {
        let id = NEXT_ACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ack = json!({"id":id,"method":"Page.screencastFrameAck","params":{"sessionId":frame["sessionId"]}});
        host.send_dev_tools_message(Some(&serde_json::to_vec(&ack).unwrap_or_default()));
    }
    let (Some(data), Some(ts)) = (frame["data"].as_str(), frame["metadata"]["timestamp"].as_f64()) else { return };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else { return };
    let path = recording.dir.join("frames").join(format!("{:06}.jpg", recording.frames.len()));
    if std::fs::write(&path, bytes).is_ok() {
        recording.frames.push((path, ts));
    }
}

fn encode(recording: Recording, end: f64) -> Result<std::path::PathBuf, String> {
    let (width, height, scale) = recording.size;
    let events = json!([{"t":0,"type":"meta","width":width,"height":height,"scale":scale}]);
    std::fs::write(recording.dir.join("events.json"), serde_json::to_string_pretty(&events).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let at: Vec<f64> = recording.frames.iter().map(|(_, ts)| (ts - recording.t0).max(0.)).collect();
    let mut list = vec!["ffconcat version 1.0".to_string()];
    for (i, (file, _)) in recording.frames.iter().enumerate() {
        let from = if i == 0 { 0. } else { at[i] };
        let to = at.get(i + 1).copied().unwrap_or(end - recording.t0);
        list.push(format!("file '{}'", file.display()));
        list.push(format!("duration {:.4}", (to - from).max(0.001)));
    }
    let last = recording.frames.last().ok_or("No frames were captured")?;
    list.push(format!("file '{}'", last.0.display()));
    let concat = recording.dir.join("frames.txt");
    std::fs::write(&concat, list.join("\n")).map_err(|e| e.to_string())?;
    let video = recording.dir.join("raw.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&concat)
        .args(["-vf", "fps=60,pad=ceil(iw/2)*2:ceil(ih/2)*2", "-c:v", "libx264", "-preset", "medium", "-crf", "12", "-pix_fmt", "yuv420p"])
        .arg(&video)
        .status()
        .map_err(|e| format!("ffmpeg is needed to encode recordings: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg failed; frames kept in {}", recording.dir.display()));
    }
    let _ = std::fs::remove_dir_all(recording.dir.join("frames"));
    let _ = std::fs::remove_file(concat);
    Ok(video)
}

type Reply = async_channel::Sender<Result<Value, String>>;
wrap_dev_tools_message_observer! {
    struct Observer { pending: Rc<RefCell<HashMap<i32, Reply>>>, epoch: Rc<Cell<u64>>, recording: Rc<RefCell<Option<Recording>>> }
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, _browser: Option<&mut Browser>, id: i32, success: i32, result: Option<&[u8]>) {
            if let Some(tx) = self.pending.borrow_mut().remove(&id) {
                let value = serde_json::from_slice(result.unwrap_or(b"{}"));
                let answer = match value {
                    Ok(v) if success != 0 => Ok(v),
                    Ok(v) => Err(format!("Chromium: {v}")),
                    Err(e) => Err(format!("Invalid Chromium reply: {e}")),
                };
                let _ = tx.try_send(answer);
            }
        }
        fn on_dev_tools_event(&self, browser: Option<&mut Browser>, method: Option<&CefString>, params: Option<&[u8]>) {
            if method.is_some_and(|m| m.to_string() == "Page.screencastFrame") {
                if let (Some(recording), Some(params)) = (self.recording.borrow_mut().as_mut(), params) {
                    save_frame(recording, params, browser);
                }
                return;
            }
            if method.is_some_and(|m| matches!(m.to_string().as_str(),
                "DOM.documentUpdated" | "DOM.childNodeInserted" | "DOM.childNodeRemoved" |
                "DOM.attributeModified" | "DOM.attributeRemoved" | "DOM.characterDataModified" |
                "DOM.shadowRootPushed" | "DOM.shadowRootPopped" | "DOM.distributedNodesUpdated")) {
                self.epoch.set(self.epoch.get().wrapping_add(1));
            }
        }
        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
            for (_, tx) in self.pending.borrow_mut().drain() {
                let _ = tx.try_send(Err("Browser closed or DevTools detached; snapshot again".into()));
            }
        }
    }
}

#[derive(Default)]
struct Snapshot {
    generation: u64,
    version: Value,
    refs: HashMap<String, i64>,
}

pub struct Control {
    host: BrowserHost,
    pending: Rc<RefCell<HashMap<i32, Reply>>>,
    next_id: Cell<i32>,
    epoch: Rc<Cell<u64>>,
    _registration: Registration,
    snapshot: RefCell<Snapshot>,
    busy: Cell<bool>,
    pointer: Box<dyn Fn(Option<&str>, f64, f64) -> bool>,
    caller: RefCell<Option<String>>,
    cursor: fn(Option<&str>) -> Vec<u8>,
    recording: Rc<RefCell<Option<Recording>>>,
}
struct Busy<'a>(&'a Cell<bool>);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl Control {
    pub fn new(browser: &Browser, pointer: Box<dyn Fn(Option<&str>, f64, f64) -> bool>, cursor: fn(Option<&str>) -> Vec<u8>) -> Result<Self, String> {
        let host = browser.host().ok_or("Browser is closed")?;
        let pending = Rc::new(RefCell::new(HashMap::new()));
        let epoch = Rc::new(Cell::new(0));
        let recording = Rc::new(RefCell::new(None));
        let mut observer = Observer::new(pending.clone(), epoch.clone(), recording.clone());
        let registration = host
            .add_dev_tools_message_observer(Some(&mut observer))
            .ok_or("Cannot observe Chromium")?;
        Ok(Self {
            host,
            pending,
            epoch,
            next_id: Cell::new(0),
            _registration: registration,
            snapshot: RefCell::new(Snapshot::default()),
            busy: Cell::new(false),
            pointer,
            caller: RefCell::new(None),
            cursor,
            recording,
        })
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        executor: &BackgroundExecutor,
    ) -> Result<Value, String> {
        let id = self
            .next_id
            .get()
            .checked_add(1)
            .ok_or("DevTools request limit reached")?;
        self.next_id.set(id);
        let (tx, rx) = async_channel::bounded(1);
        self.pending.borrow_mut().insert(id, tx);
        let message = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))
            .map_err(|e| e.to_string())?;
        if self.host.send_dev_tools_message(Some(&message)) == 0 {
            self.pending.borrow_mut().remove(&id);
            return Err("Chromium refused the command".into());
        }
        let answer = rx
            .recv()
            .with_timeout(Duration::from_secs(3), executor)
            .await;
        self.pending.borrow_mut().remove(&id);
        answer
            .map_err(|_| "Chromium command timed out; its outcome may be uncertain".to_string())?
            .map_err(|_| "Chromium observer closed".to_string())?
    }

    async fn context(&self, ex: &BackgroundExecutor) -> Result<i64, String> {
        let tree = self.call("Page.getFrameTree", json!({}), ex).await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or("No main frame")?;
        let world = self
            .call(
                "Page.createIsolatedWorld",
                json!({"frameId":frame,"worldName":"herdr-agent-control"}),
                ex,
            )
            .await?;
        world["executionContextId"]
            .as_i64()
            .ok_or("No isolated page context".into())
    }

    async fn evaluate(&self, expression: &str, ex: &BackgroundExecutor) -> Result<Value, String> {
        let context = self.context(ex).await?;
        let value = self
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"contextId":context,"returnByValue":true}),
                ex,
            )
            .await?;
        if value.get("exceptionDetails").is_some() {
            return Err("Page changed or script failed; snapshot again".into());
        }
        Ok(value["result"]["value"].clone())
    }

    async fn version(&self, ex: &BackgroundExecutor) -> Result<Value, String> {
        let context = self.context(ex).await?;
        let v = self.call("Runtime.evaluate", json!({"contextId":context,"returnByValue":true,"expression":r#"(() => {
            if (!globalThis.herdrVersion) {
                const state = globalThis.herdrVersion = {epoch: 0};
                state.observer = new MutationObserver(() => state.epoch++);
                state.observer.observe(document, {subtree:true, childList:true, attributes:true, characterData:true});
                document.addEventListener('input', () => state.epoch++, true);
                document.addEventListener('change', () => state.epoch++, true);
            }
            return {epoch:herdrVersion.epoch, url:location.href, title:document.title};
        })()"#}), ex).await?;
        if v.get("exceptionDetails").is_some() {
            return Err("Page changed; snapshot again".into());
        }
        Ok(json!({"context":context,"page":v["result"]["value"],"dom_epoch":self.epoch.get()}))
    }

    async fn node(&self, args: &Value, ex: &BackgroundExecutor) -> Result<i64, String> {
        let Some(reference) = args.get("ref").and_then(Value::as_str) else {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or("Pass ref (from a snapshot) or name")?;
            let role = args.get("role").and_then(Value::as_str);
            return self.find(name, role, args.get("nth").and_then(Value::as_u64), ex).await;
        };
        let (id, url) = {
            let s = self.snapshot.borrow();
            (
                *s.refs
                    .get(reference)
                    .ok_or("Unknown or stale ref; snapshot again")?,
                s.version["page"]["url"].clone(),
            )
        };
        if self.version(ex).await?["page"]["url"] != url {
            return Err("Page navigated; snapshot again".into());
        }
        self.connected(id, ex).await?;
        Ok(id)
    }

    async fn connected(&self, node: i64, ex: &BackgroundExecutor) -> Result<(), String> {
        let target = self
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId":node,"executionContextId":self.context(ex).await?}),
                ex,
            )
            .await
            .map_err(|_| "Element disappeared; snapshot again")?;
        let object = target["object"]["objectId"]
            .as_str()
            .ok_or("Element disappeared; snapshot again")?;
        let live = self
            .call(
                "Runtime.callFunctionOn",
                json!({"objectId":object,"functionDeclaration":"function() { return this.isConnected; }","returnByValue":true}),
                ex,
            )
            .await?;
        self.call("Runtime.releaseObject", json!({"objectId":object}), ex)
            .await?;
        if live["result"]["value"] != true {
            return Err("Element disappeared; snapshot again".into());
        }
        Ok(())
    }

    async fn find(
        &self,
        name: &str,
        role: Option<&str>,
        nth: Option<u64>,
        ex: &BackgroundExecutor,
    ) -> Result<i64, String> {
        const INTERACTIVE: &[&str] = &[
            "button", "link", "textbox", "searchbox", "combobox", "checkbox", "radio", "switch",
            "tab", "menuitem", "menuitemcheckbox", "menuitemradio", "option", "slider",
            "spinbutton", "listbox", "treeitem",
        ];
        self.call("Accessibility.enable", json!({}), ex).await?;
        let tree = self
            .call("Accessibility.getFullAXTree", json!({}), ex)
            .await?;
        let want = name.trim();
        let matches: Vec<(&str, i64)> = tree["nodes"]
            .as_array()
            .ok_or("No accessibility tree")?
            .iter()
            .filter(|n| n["ignored"] != true)
            .filter_map(|n| {
                let r = n["role"]["value"].as_str()?;
                let found = n["name"]["value"].as_str()?.trim() == want
                    && role.map_or(INTERACTIVE.contains(&r), |w| w.eq_ignore_ascii_case(r));
                Some((r, n["backendDOMNodeId"].as_i64()?)).filter(|_| found)
            })
            .collect();
        match (matches.len(), nth) {
            (0, _) => Err(format!(
                "No {} named {want:?}; snapshot to see what is there",
                role.unwrap_or("interactive element")
            )),
            (count, Some(n)) => matches
                .get((n as usize).wrapping_sub(1))
                .map(|m| m.1)
                .ok_or_else(|| format!("nth is 1-based; {count} elements match {want:?}")),
            (1, None) => Ok(matches[0].1),
            (count, None) => Err(format!(
                "{count} elements named {want:?} ({}); pass role or nth",
                matches.iter().map(|m| m.0).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    async fn field(&self, node: i64, ex: &BackgroundExecutor) -> Result<(), String> {
        let v = self
            .call("DOM.describeNode", json!({"backendNodeId":node}), ex)
            .await?;
        let attrs = v["node"]["attributes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if attrs.chunks(2).any(|pair| {
            pair.len() == 2
                && pair[0]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("type"))
                && pair[1]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("password"))
        }) {
            return Err(
                "Password fields are refused; use browser.signin with a 1Password item".into(),
            );
        }
        Ok(())
    }

    async fn editable_target(&self, node: i64, ex: &BackgroundExecutor) -> Result<(), String> {
        let target = self
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId":node,"executionContextId":self.context(ex).await?}),
                ex,
            )
            .await?;
        let object = target["object"]["objectId"]
            .as_str()
            .ok_or("Element disappeared; snapshot again")?;
        let result = self
            .call(
                "Runtime.callFunctionOn",
                json!({"objectId":object,"functionDeclaration":r#"function() {
            let e = document.activeElement;
            while (e?.shadowRoot?.activeElement) e = e.shadowRoot.activeElement;
            return (this === e || this.isContentEditable && this.contains(e)) &&
                (this.matches('input:not([type=password]),textarea') || this.isContentEditable) &&
                !this.disabled && !this.readOnly;
        }"#,"returnByValue":true}),
                ex,
            )
            .await?;
        self.call("Runtime.releaseObject", json!({"objectId":object}), ex)
            .await?;
        if result["result"]["value"] != true {
            return Err(
                "Target must remain the focused editable non-password field; snapshot again".into(),
            );
        }
        Ok(())
    }

    async fn point(&self, node: i64, ex: &BackgroundExecutor) -> Result<(f64, f64), String> {
        self.call(
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId":node}),
            ex,
        )
        .await?;
        let boxes = self
            .call("DOM.getContentQuads", json!({"backendNodeId":node}), ex)
            .await?;
        let q = boxes["quads"][0]
            .as_array()
            .filter(|q| q.len() == 8)
            .ok_or("Element has no visible box; snapshot again")?;
        let mut x = 0.;
        let mut y = 0.;
        for pair in q.chunks(2) {
            x += pair[0].as_f64().ok_or("Invalid element box")?;
            y += pair[1].as_f64().ok_or("Invalid element box")?;
        }
        Ok((x / 4., y / 4.))
    }

    async fn click(&self, node: i64, ex: &BackgroundExecutor) -> Result<(), String> {
        let (x, y) = self.point(node, ex).await?;
        // Hit-test after scrolling; never click a covering element by accident.
        let hit = self
            .call(
                "DOM.getNodeForLocation",
                json!({"x":x as i64,"y":y as i64}),
                ex,
            )
            .await?;
        let target = self
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId":node,"executionContextId":self.context(ex).await?}),
                ex,
            )
            .await?;
        let object = target["object"]["objectId"]
            .as_str()
            .ok_or("Element disappeared; snapshot again")?;
        let hit_node = hit["backendNodeId"]
            .as_i64()
            .ok_or("Element is not visible")?;
        let hit_object = self
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId":hit_node,"executionContextId":self.context(ex).await?}),
                ex,
            )
            .await?;
        let object_id = hit_object["object"]["objectId"]
            .as_str()
            .ok_or("Element disappeared")?;
        let contains = self.call("Runtime.callFunctionOn", json!({"objectId":object,"functionDeclaration":"function(other) { return this === other || this.contains(other); }","arguments":[{"objectId":object_id}],"returnByValue":true}), ex).await?;
        self.call("Runtime.releaseObject", json!({"objectId":object}), ex)
            .await?;
        self.call("Runtime.releaseObject", json!({"objectId":object_id}), ex)
            .await?;
        if contains["result"]["value"] != true {
            return Err("Element is covered; snapshot again".into());
        }
        let caller = self.caller.borrow().clone();
        if self.recording.borrow().is_some() {
            let svg = String::from_utf8_lossy(&(self.cursor)(caller.as_deref())).into_owned();
            let expression = format!("window.__herdrRecordAs?.({}, {})", json!(caller.as_deref().unwrap_or_default()), json!(svg));
            self.call("Runtime.evaluate", json!({"expression":expression}), ex).await?;
        }
        if (self.pointer)(caller.as_deref(), x, y) {
            ex.timer(Duration::from_millis(350)).await;
        }
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
                ex,
            )
            .await?;
        }
        Ok(())
    }

    async fn record_start(&self, ex: &BackgroundExecutor) -> Result<Value, String> {
        if self.recording.borrow().is_some() {
            return Err("This pane is already recording".into());
        }
        let size = self.evaluate("[innerWidth, innerHeight, devicePixelRatio]", ex).await?;
        let number = |i: usize| size[i].as_f64().filter(|n| *n > 0.).ok_or("Page has no viewport");
        let size = (number(0)?, number(1)?, number(2)?);
        let t0 = now();
        let dir = crate::mods::app_dir().join("recordings").join(format!("{}", (t0 * 1000.) as u64));
        std::fs::create_dir_all(dir.join("frames")).map_err(|e| e.to_string())?;
        let source = RECORD_CURSOR.replace("__SVG__", &serde_json::to_string(&String::from_utf8_lossy(&(self.cursor)(None))).unwrap_or_default());
        self.call("Page.enable", json!({}), ex).await?;
        let script = self
            .call("Page.addScriptToEvaluateOnNewDocument", json!({"source":source}), ex)
            .await?["identifier"]
            .as_str()
            .map(str::to_string);
        self.call("Runtime.evaluate", json!({"expression":source}), ex).await?;
        *self.recording.borrow_mut() = Some(Recording { dir: dir.clone(), t0, size, script, frames: Vec::new() });
        let started = self
            .call("Page.startScreencast", json!({"format":"jpeg","quality":90,"maxWidth":(size.0 * size.2) as i64,"maxHeight":(size.1 * size.2) as i64,"everyNthFrame":1}), ex)
            .await;
        if let Err(error) = started {
            self.recording.borrow_mut().take();
            return Err(error);
        }
        Ok(json!({"recording":true,"dir":dir}))
    }

    async fn record_stop(&self, ex: &BackgroundExecutor) -> Result<Value, String> {
        let recording = self.recording.borrow_mut().take().ok_or("This pane is not recording")?;
        let end = now();
        let _ = self.call("Page.stopScreencast", json!({}), ex).await;
        if let Some(id) = &recording.script {
            let _ = self.call("Page.removeScriptToEvaluateOnNewDocument", json!({"identifier":id}), ex).await;
        }
        let _ = self.call("Runtime.evaluate", json!({"expression":"window.__herdrRecordStop?.()"}), ex).await;
        let dir = recording.dir.clone();
        let frames = recording.frames.len();
        if frames == 0 {
            return Err("No frames were captured; is the pane visible?".into());
        }
        ex.spawn(async move {
            match encode(recording, end) {
                Ok(video) => {
                    let _ = std::process::Command::new("open").arg("-R").arg(video).status();
                }
                Err(error) => tracing::warn!("browser recording: {error}"),
            }
        })
        .detach();
        Ok(json!({"recording":false,"dir":dir,"video":dir.join("raw.mp4"),"frames":frames,"encoding":true}))
    }

    async fn press(&self, key: &str, ex: &BackgroundExecutor) -> Result<(), String> {
        if self.evaluate("(() => { let e=document.activeElement; while(e?.shadowRoot?.activeElement) e=e.shadowRoot.activeElement; return !!e?.matches('input[type=password],iframe,frame,object,embed'); })()", ex).await? == true {
            return Err("Password fields are refused; use browser.signin".into());
        }
        let tree = self
            .call("Accessibility.getFullAXTree", json!({}), ex)
            .await?;
        if let Some(nodes) = tree["nodes"].as_array() {
            for node in nodes {
                let focused = node["properties"].as_array().is_some_and(|properties| {
                    properties
                        .iter()
                        .any(|p| p["name"] == "focused" && p["value"]["value"] == true)
                });
                if focused {
                    if let Some(id) = node["backendDOMNodeId"].as_i64() {
                        self.field(id, ex).await?;
                    }
                }
            }
        }
        self.send_key(key, ex).await
    }

    async fn send_key(&self, key: &str, ex: &BackgroundExecutor) -> Result<(), String> {
        let mut modifiers = 0;
        let parts: Vec<_> = key.split('+').collect();
        for part in &parts[..parts.len() - 1] {
            modifiers |= match part.to_ascii_lowercase().as_str() {
                "alt" => 1,
                "ctrl" | "control" => 2,
                "meta" | "cmd" => 4,
                "shift" => 8,
                _ => return Err("Unknown key modifier".into()),
            };
        }
        let k = *parts.last().unwrap_or(&"");
        let (key, code, vk): (String, String, i32) = match k.to_ascii_lowercase().as_str() {
            "enter" => ("Enter".into(),"Enter".into(),13), "tab" => ("Tab".into(),"Tab".into(),9),
            "escape"|"esc" => ("Escape".into(),"Escape".into(),27), "backspace" => ("Backspace".into(),"Backspace".into(),8),
            "delete" => ("Delete".into(),"Delete".into(),46), "arrowleft" => ("ArrowLeft".into(),"ArrowLeft".into(),37),
            "arrowup" => ("ArrowUp".into(),"ArrowUp".into(),38), "arrowright" => ("ArrowRight".into(),"ArrowRight".into(),39),
            "arrowdown" => ("ArrowDown".into(),"ArrowDown".into(),40), "home" => ("Home".into(),"Home".into(),36), "end" => ("End".into(),"End".into(),35),
            "pageup" => ("PageUp".into(),"PageUp".into(),33), "pagedown" => ("PageDown".into(),"PageDown".into(),34),
            "space" => (" ".into(),"Space".into(),32),
            _ if k.chars().count()==1 && k.is_ascii() => (k.to_string(), format!("Key{}",k.to_ascii_uppercase()),k.as_bytes()[0].to_ascii_uppercase() as i32),
            _ => return Err("Unsupported key; use Enter, Tab, Escape, arrows, or an ASCII character with optional Ctrl/Alt/Meta/Shift".into()),
        };
        let text = if modifiers & !8 == 0 && key.len() == 1 {
            key.clone()
        } else if key == "Enter" {
            "\r".into()
        } else {
            String::new()
        };
        let commands: Vec<&str> = if modifiers == 4 && key.eq_ignore_ascii_case("a") {
            vec!["selectAll"]
        } else {
            vec![]
        };
        self.call("Input.dispatchKeyEvent", json!({"type":"keyDown","key":key,"code":code,"windowsVirtualKeyCode":vk,"modifiers":modifiers,"text":text,"commands":commands}), ex).await?;
        self.call("Input.dispatchKeyEvent", json!({"type":"keyUp","key":key,"code":code,"windowsVirtualKeyCode":vk,"modifiers":modifiers}), ex).await?;
        Ok(())
    }

    async fn page_host(&self, ex: &BackgroundExecutor) -> Result<String, String> {
        let page = self.version(ex).await?;
        let url = url::Url::parse(page["page"]["url"].as_str().unwrap_or(""))
            .map_err(|_| "Sign-in needs a web page")?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err("Sign-in needs an http(s) page".into());
        }
        url.host_str()
            .map(str::to_ascii_lowercase)
            .ok_or("Sign-in needs a page with a host".into())
    }

    async fn login_field(&self, password: bool, ex: &BackgroundExecutor) -> Result<Option<i64>, String> {
        let selectors = if password {
            json!(["input[type=password]"])
        } else {
            json!([
                "input[autocomplete~=username]", "input[type=email]", "input[name=identifier]",
                "input[name*=user i]", "input[id*=user i]", "input[name*=login i]", "input[name*=email i]"
            ])
        };
        self.call("DOM.enable", json!({}), ex).await?;
        let context = self.context(ex).await?;
        let found = self.call("Runtime.evaluate", json!({"contextId":context,"returnByValue":false,"expression":format!(
            "(() => {{ for (const s of {selectors}) for (const e of document.querySelectorAll(s)) if (e.getClientRects().length && !e.disabled && !e.readOnly) return e; return null; }})()"
        )}), ex).await?;
        let Some(object) = found["result"]["objectId"].as_str() else {
            return Ok(None);
        };
        let node = self
            .call("DOM.describeNode", json!({"objectId":object}), ex)
            .await;
        self.call("Runtime.releaseObject", json!({"objectId":object}), ex)
            .await?;
        Ok(node?["node"]["backendNodeId"].as_i64())
    }

    /// Checks the site again right before each value, so a redirect never receives it.
    async fn fill(&self, node: i64, sites: &[String], value: &str, ex: &BackgroundExecutor) -> Result<(), String> {
        let host = self.page_host(ex).await?;
        if !sites.iter().any(|site| host == *site || host.ends_with(&format!(".{site}"))) {
            return Err(format!("The 1Password item does not list {host}; refusing to fill"));
        }
        if value.is_empty() {
            return Err("The 1Password item has an empty value for this field".into());
        }
        self.click(node, ex).await?;
        let target = self
            .call(
                "DOM.resolveNode",
                json!({"backendNodeId":node,"executionContextId":self.context(ex).await?}),
                ex,
            )
            .await?;
        let object = target["object"]["objectId"]
            .as_str()
            .ok_or("Login field disappeared")?;
        self.call("Runtime.callFunctionOn", json!({"objectId":object,"functionDeclaration":"function() { this.focus(); this.select?.(); }"}), ex)
            .await?;
        self.call("Runtime.releaseObject", json!({"objectId":object}), ex)
            .await?;
        self.call("Input.insertText", json!({"text":value}), ex).await?;
        Ok(())
    }

    pub async fn run(
        &self,
        command: &str,
        args: &Value,
        ex: &BackgroundExecutor,
    ) -> Result<Value, String> {
        if self.busy.replace(true) {
            return Err("Another command is running in this pane; wait for its reply".into());
        }
        let _busy = Busy(&self.busy);
        *self.caller.borrow_mut() = args["caller"].as_str().map(str::to_owned);
        match command {
            "browser.record" => match args["action"].as_str() {
                Some("start") => self.record_start(ex).await,
                Some("stop") => self.record_stop(ex).await,
                _ => Err("action must be start or stop".into()),
            },
            "browser.cdp" => {
                // The pane's own target names the agent profile the bridge exposes.
                let info = self.call("Target.getTargetInfo", json!({}), ex).await?;
                let target = &info["targetInfo"];
                let context = target["browserContextId"]
                    .as_str()
                    .ok_or("Chromium did not name this page's profile")?;
                let endpoint = super::cdp::endpoint(context, args["pane"].as_str())?;
                Ok(json!({"endpoint":endpoint,"targetId":target["targetId"],"type":target["type"],"url":target["url"]}))
            }
            "browser.snapshot" => {
                // A failed snapshot invalidates all previous refs, too.
                self.snapshot.borrow_mut().refs.clear();
                self.call("DOM.enable", json!({}), ex).await?;
                // Request all descendants so native DOM events also cover closed
                // shadow roots and frame documents, outside a document observer.
                self.call("DOM.getDocument", json!({"depth":-1,"pierce":true}), ex)
                    .await?;
                let before = self.version(ex).await?;
                self.call("Accessibility.enable", json!({}), ex).await?;
                let tree = self
                    .call("Accessibility.getFullAXTree", json!({}), ex)
                    .await?;
                let nodes = tree["nodes"].as_array().ok_or("No accessibility tree")?;
                let mut scope_id = None;
                if let Some(scope) = args.get("scope").and_then(Value::as_str) {
                    let doc = self.call("DOM.getDocument", json!({}), ex).await?;
                    let found = self
                        .call(
                            "DOM.querySelector",
                            json!({"nodeId":doc["root"]["nodeId"],"selector":scope}),
                            ex,
                        )
                        .await?;
                    let node = self
                        .call("DOM.describeNode", json!({"nodeId":found["nodeId"]}), ex)
                        .await?;
                    let backend = node["node"]["backendNodeId"]
                        .as_i64()
                        .ok_or("Scope did not match an element")?;
                    scope_id = Some(
                        nodes
                            .iter()
                            .find(|n| n["backendDOMNodeId"].as_i64() == Some(backend))
                            .and_then(|n| n["nodeId"].as_str())
                            .ok_or("Scope is absent from the accessibility tree")?
                            .to_string(),
                    );
                }
                let after = self.version(ex).await?;
                if before["page"]["url"] != after["page"]["url"] {
                    return Err("Page navigated while reading; snapshot again".into());
                }
                let by_id: HashMap<_, _> = nodes
                    .iter()
                    .filter_map(|n| Some((n["nodeId"].as_str()?, n)))
                    .collect();
                let mut pending = vec![
                    scope_id
                        .as_deref()
                        .or_else(|| nodes.first().and_then(|n| n["nodeId"].as_str()))
                        .ok_or("Empty accessibility tree")?,
                ];
                let mut lines = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut s = self.snapshot.borrow_mut();
                s.generation += 1;
                s.version = after.clone();
                while let Some(id) = pending.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let Some(n) = by_id.get(id) else {
                        continue;
                    };
                    if let Some(children) = n["childIds"].as_array() {
                        pending.extend(children.iter().rev().filter_map(Value::as_str));
                    }
                    if n["ignored"] == true {
                        continue;
                    }
                    let role = n["role"]["value"].as_str().unwrap_or("unknown");
                    let name: String = n["name"]["value"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(300)
                        .collect();
                    let reference = if let Some(backend) = n["backendDOMNodeId"].as_i64() {
                        let r = format!(
                            "e{}",
                            NEXT_REF.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        );
                        s.refs.insert(r.clone(), backend);
                        r
                    } else {
                        "-".into()
                    };
                    lines.push(format!(
                        "{reference} {role} {}",
                        serde_json::to_string(&name).unwrap()
                    ));
                    if lines.len() >= 1000 {
                        break;
                    }
                }
                Ok(
                    json!({"url":after["page"]["url"],"title":after["page"]["title"],"generation":s.generation,"tree":lines.join("\n"),"capped":!pending.is_empty()}),
                )
            }
            "browser.read" => {
                let format = args["format"].as_str().unwrap_or("text");
                if !matches!(format, "text" | "markdown") {
                    return Err("format must be text or markdown".into());
                }
                // innerText includes only rendered text; no HTML, hidden inputs, or form values.
                // Markdown is deliberately plain text (valid Markdown), escaping syntax below.
                let mut result = self.evaluate("({url:location.href,title:document.title,text:(document.body?.innerText || '').slice(0,20000),capped:(document.body?.innerText.length || 0)>20000})", ex).await?;
                if format == "markdown" {
                    let text = result["text"].as_str().unwrap_or("");
                    let escaped: String = text
                        .chars()
                        .flat_map(|c| {
                            if "\\`*_{}[]<>#+-.!|".contains(c) {
                                vec!['\\', c]
                            } else {
                                vec![c]
                            }
                        })
                        .collect();
                    result["text"] = escaped.into();
                }
                result["format"] = format.into();
                Ok(result)
            }
            "browser.click" => {
                let node = self.node(args, ex).await?;
                self.click(node, ex).await?;
                Ok(json!({"ok":true,"url":self.version(ex).await?["page"]["url"]}))
            }
            "browser.type" => {
                let text = args["text"].as_str().ok_or("text is required")?;
                if text.len() > 20000 {
                    return Err("text exceeds 20000 bytes".into());
                }
                let node = self.node(args, ex).await?;
                self.field(node, ex).await?;
                self.click(node, ex).await?;
                self.editable_target(node, ex).await?;
                self.press("Meta+a", ex).await?;
                self.editable_target(node, ex).await?;
                self.call("Input.insertText", json!({"text":text}), ex)
                    .await?;
                if args["submit"] == true {
                    self.press("Enter", ex).await?;
                }
                Ok(json!({"ok":true}))
            }
            "browser.signin" => {
                let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                self.page_host(ex).await?;
                let login = match (text("item"), text("env_file")) {
                    (Some(item), None) if !item.starts_with('-') => ex.spawn(async move { one_password(&item) }).await?,
                    (None, Some(file)) => {
                        let user = text("user_key").ok_or("env_file needs user_key")?;
                        let password = text("password_key").ok_or("env_file needs password_key")?;
                        let site = text("site").ok_or("env_file needs site, the host it may fill (e.g. localhost)")?;
                        env_login(&file, &user, &password, &site)?
                    }
                    _ => return Err("Pass exactly one of item (1Password) or env_file".into()),
                };
                let mut filled = Vec::new();
                if let Some(field) = self.login_field(false, ex).await? {
                    self.fill(field, &login.urls, &login.username, ex).await?;
                    filled.push("username");
                }
                let mut password = self.login_field(true, ex).await?;
                if password.is_none() && !filled.is_empty() {
                    self.send_key("Enter", ex).await?;
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while password.is_none() && Instant::now() < deadline {
                        ex.timer(Duration::from_millis(250)).await;
                        password = self.login_field(true, ex).await.unwrap_or(None);
                    }
                }
                match password {
                    Some(field) => {
                        self.fill(field, &login.urls, &login.password, ex).await?;
                        filled.push("password");
                        self.send_key("Enter", ex).await?;
                    }
                    None if filled.is_empty() => {
                        return Err("No visible login fields on this page".into());
                    }
                    None => {}
                }
                Ok(json!({"ok":true,"filled":filled,"url":self.version(ex).await?["page"]["url"]}))
            }
            "browser.press" => {
                self.press(args["key"].as_str().ok_or("key is required")?, ex)
                    .await?;
                Ok(json!({"ok":true}))
            }
            "browser.scroll" => {
                if args.get("ref").is_some() == args.get("dy").is_some() {
                    return Err("Pass exactly one of ref or dy".into());
                }
                if args.get("ref").is_some() {
                    let node = self.node(args, ex).await?;
                    self.call(
                        "DOM.scrollIntoViewIfNeeded",
                        json!({"backendNodeId":node}),
                        ex,
                    )
                    .await?;
                } else {
                    let dy = args["dy"]
                        .as_f64()
                        .filter(|n| n.is_finite() && n.abs() <= 100000.)
                        .ok_or("dy must be a finite number within 100000 pixels")?;
                    self.call(
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseWheel","x":1,"y":1,"deltaX":0,"deltaY":dy}),
                        ex,
                    )
                    .await?;
                }
                Ok(json!({"ok":true}))
            }
            "browser.wait" => {
                let count = ["text", "url", "load"]
                    .iter()
                    .filter(|k| args.get(**k).is_some())
                    .count();
                if count != 1 {
                    return Err("Pass exactly one of text, url, or load".into());
                }
                if args.get("text").is_some_and(|v| !v.is_string())
                    || args.get("url").is_some_and(|v| !v.is_string())
                    || args.get("load").is_some_and(|v| v != true)
                {
                    return Err("text/url must be strings; load must be true".into());
                }
                let timeout = match args.get("timeout") {
                    None => 5000,
                    Some(v) => v
                        .as_u64()
                        .filter(|v| *v <= 120000)
                        .ok_or("timeout must be 0..120000 milliseconds")?,
                };
                let deadline = Instant::now() + Duration::from_millis(timeout);
                loop {
                    let condition = if let Some(text) = args.get("text") {
                        format!("(document.body?.innerText || '').includes({text})")
                    } else if let Some(url) = args.get("url") {
                        format!("location.href.includes({url})")
                    } else {
                        "document.readyState === 'complete'".into()
                    };
                    // A navigation can destroy a context between polling reads.
                    // Only reads are retried; input commands are never replayed.
                    if self.evaluate(&condition, ex).await.is_ok_and(|v| v == true) {
                        return Ok(json!({"ok":true}));
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"ok":false,"timeout":true}));
                    }
                    ex.timer(Duration::from_millis(100)).await;
                }
            }
            "browser.screenshot" => {
                let shot = self
                    .call(
                        "Page.captureScreenshot",
                        json!({"format":"png","captureBeyondViewport":false}),
                        ex,
                    )
                    .await?;
                let encoded = shot["data"]
                    .as_str()
                    .filter(|s| s.len() <= 48 * 1024 * 1024)
                    .ok_or("No screenshot or capture too large")?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|e| e.to_string())?;
                let dir = crate::mods::app_dir().join("screenshots");
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let path = dir.join(format!(
                    "browser-{}-{}.png",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|e| e.to_string())?
                        .as_nanos()
                ));
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| e.to_string())?;
                file.write_all(&bytes).map_err(|e| e.to_string())?;
                Ok(json!({"path":path}))
            }
            _ => Err(format!("Unknown browser command: {command}")),
        }
    }
}

const RECORD_CURSOR: &str = r#"(() => {
  if (window.__herdrRecordStop) return;
  const cursors = new Map();
  let current = '', svg = __SVG__;
  const place = (c) => { c.el.style.transform = `translate(${c.x - 4.5}px, ${c.y - 3}px)`; };
  const tick = (c) => {
    c.x += (c.tx - c.x) * 0.45; c.y += (c.ty - c.y) * 0.45;
    if (Math.abs(c.tx - c.x) < 0.3 && Math.abs(c.ty - c.y) < 0.3) { c.x = c.tx; c.y = c.ty; c.moving = false; }
    place(c);
    if (c.moving) requestAnimationFrame(() => tick(c));
  };
  const mount = () => {
    let c = cursors.get(current);
    if (c?.el.isConnected || !document.documentElement) return c;
    const el = document.createElement('div');
    el.style.cssText = 'position:fixed;left:0;top:0;width:28px;height:28px;z-index:2147483647;pointer-events:none;opacity:0;transition:opacity .4s';
    el.innerHTML = svg;
    document.documentElement.appendChild(el);
    c = { el, x: 0, y: 0, tx: 0, ty: 0, moving: false, seen: false, timer: 0 };
    cursors.set(current, c);
    return c;
  };
  const to = (e) => {
    const c = mount();
    if (!c) return;
    c.tx = e.clientX; c.ty = e.clientY;
    if (!c.seen) { c.seen = true; c.x = c.tx; c.y = c.ty; place(c); }
    c.el.style.opacity = '1';
    clearTimeout(c.timer);
    c.timer = setTimeout(() => { c.el.style.opacity = '0'; }, 5000);
    if (!c.moving) { c.moving = true; requestAnimationFrame(() => tick(c)); }
  };
  const ripple = (e) => {
    to(e);
    const r = document.createElement('div');
    r.style.cssText = `position:fixed;left:${e.clientX - 14}px;top:${e.clientY - 14}px;width:28px;height:28px;border-radius:50%;border:2px solid rgba(0,0,0,.4);box-shadow:0 0 0 1px rgba(255,255,255,.6);z-index:2147483646;pointer-events:none;transition:transform .45s ease-out,opacity .45s ease-out`;
    document.documentElement.appendChild(r);
    requestAnimationFrame(() => { r.style.transform = 'scale(1.9)'; r.style.opacity = '0'; });
    setTimeout(() => r.remove(), 500);
  };
  addEventListener('mousemove', to, true);
  addEventListener('mousedown', ripple, true);
  window.__herdrRecordAs = (key, next) => { current = key; svg = next; };
  window.__herdrRecordStop = () => {
    removeEventListener('mousemove', to, true);
    removeEventListener('mousedown', ripple, true);
    for (const c of cursors.values()) c.el.remove();
    delete window.__herdrRecordAs;
    delete window.__herdrRecordStop;
  };
})()"#;

struct Login {
    username: String,
    password: String,
    urls: Vec<String>,
}

/// `op` asks the person to approve (Touch ID) on first use; its output never reaches a reply or log.
fn one_password(item: &str) -> Result<Login, String> {
    let output = ["op", "/opt/homebrew/bin/op", "/usr/local/bin/op"]
        .into_iter()
        .find_map(|op| {
            std::process::Command::new(op)
                .args(["item", "get", item, "--format", "json", "--reveal"])
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
        })
        .ok_or("1Password CLI `op` is not installed")?;
    if !output.status.success() {
        return Err(format!(
            "op could not read {item:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let v: Value = serde_json::from_slice(&output.stdout).map_err(|_| "op returned unreadable output")?;
    let field = |purpose: &str| {
        v["fields"]
            .as_array()
            .and_then(|f| f.iter().find(|f| f["purpose"] == purpose))
            .and_then(|f| f["value"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let urls: Vec<String> = v["urls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["href"].as_str())
        .filter_map(|href| {
            let href = if href.contains("://") { href.to_string() } else { format!("https://{href}") };
            url::Url::parse(&href).ok()?.host_str().map(str::to_ascii_lowercase)
        })
        .collect();
    if urls.is_empty() {
        return Err(format!("1Password item {item:?} lists no website; add one so sign-in knows where it may fill"));
    }
    Ok(Login { username: field("USERNAME"), password: field("PASSWORD"), urls })
}

/// Test credentials from a dotenv file; `site` stands in for a 1Password item's websites.
fn env_login(file: &str, user_key: &str, password_key: &str, site: &str) -> Result<Login, String> {
    let body = std::fs::read_to_string(file).map_err(|e| format!("Cannot read {file}: {e}"))?;
    let value = |key: &str| {
        body.lines()
            .filter_map(|line| line.trim().trim_start_matches("export ").split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
            .ok_or(format!("{file} has no {key}"))
    };
    Ok(Login {
        username: value(user_key)?,
        password: value(password_key)?,
        urls: vec![site.trim().to_ascii_lowercase()],
    })
}
