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

type Reply = async_channel::Sender<Result<Value, String>>;
wrap_dev_tools_message_observer! {
    struct Observer { pending: Rc<RefCell<HashMap<i32, Reply>>>, epoch: Rc<Cell<u64>> }
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
        fn on_dev_tools_event(&self, _browser: Option<&mut Browser>, method: Option<&CefString>, _params: Option<&[u8]>) {
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
}
struct Busy<'a>(&'a Cell<bool>);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl Control {
    pub fn new(browser: &Browser) -> Result<Self, String> {
        let host = browser.host().ok_or("Browser is closed")?;
        let pending = Rc::new(RefCell::new(HashMap::new()));
        let epoch = Rc::new(Cell::new(0));
        let mut observer = Observer::new(pending.clone(), epoch.clone());
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
        let reference = args["ref"]
            .as_str()
            .ok_or("ref is required; snapshot again")?;
        let (id, version) = {
            let s = self.snapshot.borrow();
            (
                *s.refs
                    .get(reference)
                    .ok_or("Unknown or stale ref; snapshot again")?,
                s.version.clone(),
            )
        };
        if self.version(ex).await? != version {
            return Err("Page changed; snapshot again".into());
        }
        Ok(id)
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
                "Password fields require the sign-in vault; browser.type refuses them".into(),
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

    async fn press(&self, key: &str, ex: &BackgroundExecutor) -> Result<(), String> {
        if self.evaluate("(() => { let e=document.activeElement; while(e?.shadowRoot?.activeElement) e=e.shadowRoot.activeElement; return !!e?.matches('input[type=password],iframe,frame,object,embed'); })()", ex).await? == true {
            return Err("Password fields require the sign-in vault".into());
        }
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
        self.call("Input.dispatchKeyEvent", json!({"type":"keyDown","key":key,"code":code,"windowsVirtualKeyCode":vk,"modifiers":modifiers,"text":text}), ex).await?;
        self.call("Input.dispatchKeyEvent", json!({"type":"keyUp","key":key,"code":code,"windowsVirtualKeyCode":vk,"modifiers":modifiers}), ex).await?;
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
        match command {
            "browser.snapshot" => {
                // A failed snapshot invalidates all previous refs, too.
                self.snapshot.borrow_mut().refs.clear();
                self.call("DOM.enable", json!({}), ex).await?;
                // Request all descendants so native DOM events also cover closed
                // shadow roots and frame documents, outside a document observer.
                self.call("DOM.getDocument", json!({"depth":-1,"pierce":true}), ex).await?;
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
                if before != after {
                    return Err("Page changed while reading; snapshot again".into());
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
                        .filter(|v| *v <= 30000)
                        .ok_or("timeout must be 0..30000 milliseconds")?,
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
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos()
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
