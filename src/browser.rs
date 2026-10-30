//! A native browser hosted by the window, using existing mod-pane identities.
//! Website JavaScript never receives a bridge to Herdr's privileged mod runtime.
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_base::input::{Input, InputEvent, InputState};
use serde_json::{Value, json};
use std::cell::Cell;
use std::rc::Rc;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub mod runtime;

pub const MOD: &str = "native-browser";

#[derive(Debug)]
pub enum Event {
    Loading(String),
    Loaded(String),
    Title(String),
    Open(String),
    Command(&'static str),
    FocusUrl,
    Focused,
    Notice(String),
    Error(String),
}

/// Normalize address-bar input, while keeping non-web schemes out of navigation.
pub fn address(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() || text == "about:blank" {
        return Ok("about:blank".into());
    }
    let local =
        text.starts_with("localhost") || text.starts_with("127.") || text.starts_with("[::1]");
    let candidate = if local {
        format!("http://{text}")
    } else if text.contains("://")
        || text.starts_with("javascript:")
        || text.starts_with("data:")
        || text.starts_with("file:")
        || text.starts_with("about:")
    {
        text.into()
    } else if !text.chars().any(char::is_whitespace) && text.contains('.') {
        format!("https://{text}")
    } else {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", text)
            .finish();
        format!("https://duckduckgo.com/?{query}")
    };
    let parsed = url::Url::parse(&candidate).map_err(|e| format!("Invalid address: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Only HTTP and HTTPS pages can be opened here.".into());
    }
    Ok(parsed.into())
}

pub struct Browser {
    pane: String,
    input: Entity<InputState>,
    url: String,
    title: String,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    allowed: Rc<Cell<bool>>,
    #[cfg(target_os = "macos")]
    host: Option<Rc<macos::Host>>,
    _input: Subscription,
    _events: Task<()>,
}

impl Browser {
    pub fn new(pane: &str, props: &Value, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = address(
            props
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("https://www.google.com"),
        )
        .unwrap_or_else(|_| "about:blank".into());
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter an address"));
        input.update(cx, |i, cx| i.set_value(url.clone(), window, cx));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let text = this.input.read(cx).value().to_string();
                let _ = this.navigate(&text, window, cx);
                this.focus_page();
            }
        });
        let (tx, rx) = async_channel::unbounded();
        let allowed = Rc::new(Cell::new(false));
        #[cfg(target_os = "macos")]
        let (host, error) = match macos::Host::new(&url, allowed.clone(), tx.clone(), window) {
            Ok(h) => (Some(Rc::new(h)), None),
            Err(e) => (None, Some(format!("Cannot start Chromium: {e:#}"))),
        };
        #[cfg(not(target_os = "macos"))]
        let error = Some("Native browser panes currently require macOS.".into());
        let events = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            pane: pane.into(),
            input,
            url,
            title: "Browser".into(),
            loading: true,
            error,
            notice: None,
            allowed,
            #[cfg(target_os = "macos")]
            host,
            _input: subscription,
            _events: events,
        }
    }

    fn event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            Event::Loading(url) => {
                self.loading = true;
                self.url = url;
                if !self.input.read(cx).focus_handle(cx).is_focused(window) {
                    self.input
                        .update(cx, |i, cx| i.set_value(self.url.clone(), window, cx));
                }
            }
            Event::Loaded(url) => {
                self.loading = false;
                self.url = url;
                if !self.input.read(cx).focus_handle(cx).is_focused(window) {
                    self.input
                        .update(cx, |i, cx| i.set_value(self.url.clone(), window, cx));
                }
            }
            Event::Title(title) => self.title = title,
            Event::Open(url) => {
                crate::mods::request(crate::mods::Request::Run {
                    name: "browser.open".into(),
                    args: json!({"url":url}),
                });
            }
            Event::Command(command) => {
                if matches!(
                    command,
                    "browser.back" | "browser.forward" | "browser.reload" | "browser.devtools"
                ) {
                    let _ = self.command(command, &Value::Null, window, cx);
                } else {
                    crate::mods::request(crate::mods::Request::Run {
                        name: command.into(),
                        args: Value::Null,
                    });
                }
            }
            Event::FocusUrl => self.focus_url(window, cx),
            Event::Focused => {
                crate::mods::request(crate::mods::Request::Run {
                    name: "pane.focus".into(),
                    args: json!({"pane":self.pane}),
                });
            }
            Event::Notice(message) => {
                self.loading = false;
                self.notice = Some(message);
            }
            Event::Error(message) => {
                self.loading = false;
                self.error = Some(message);
            }
        }
        cx.notify();
    }

    pub fn set_allowed(&self, allowed: bool) {
        self.allowed.set(allowed);
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            host.set_allowed(allowed);
        }
    }

    pub fn is_focused(&self) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            return host.is_focused();
        }
        false
    }

    pub fn focus_page(&self) {
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            host.focus();
        }
    }

    fn focus_url(&self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            host.focus_parent();
        }
        self.input.update(cx, |i, cx| {
            i.focus(window, cx);
            i.select_all(window, cx);
        });
    }

    fn navigate(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let url = match address(text) {
            Ok(url) => url,
            Err(error) => {
                self.error = Some(error.clone());
                cx.notify();
                return Err(error);
            }
        };
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            host.navigate(&url).map_err(|e| e.to_string())?;
        }
        self.url = url.clone();
        self.loading = true;
        self.error = None;
        self.notice = None;
        self.input
            .update(cx, |i, cx| i.set_value(url.clone(), window, cx));
        cx.notify();
        Ok(json!({"url":url}))
    }

    pub fn inspect(&self) -> Value {
        let mut out = json!({"engine":"Chromium/CEF", "pane":self.pane, "url":self.url, "title":self.title, "loading":self.loading, "page_error":self.error, "notice":self.notice, "visible":self.allowed.get()});
        #[cfg(target_os = "macos")]
        if let Some(host) = &self.host {
            out["native"] = host.inspect();
        }
        out
    }

    pub fn command(
        &mut self,
        command: &str,
        args: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        match command {
            "browser.navigate" => {
                return self.navigate(
                    args.get("url")
                        .and_then(Value::as_str)
                        .ok_or("url is required")?,
                    window,
                    cx,
                );
            }
            "browser.focus_url" => self.focus_url(window, cx),
            "browser.inspect" => return Ok(self.inspect()),
            #[cfg(target_os = "macos")]
            _ => {
                if let Some(host) = &self.host {
                    host.command(command)?;
                }
            }
            #[cfg(not(target_os = "macos"))]
            _ => return Err("Native browsers require macOS".into()),
        }
        cx.notify();
        Ok(json!({"ok":true}))
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = crate::settings::color;
        let button = |id: &'static str, label: &'static str, command: &'static str| {
            div()
                .id(id)
                .w(px(30.))
                .h(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .text_color(color("foreground"))
                .hover(|s| s.bg(color("hover")))
                .on_click(cx.listener(move |this, _, w, cx| {
                    let _ = this.command(command, &Value::Null, w, cx);
                }))
                .child(label)
        };
        let toolbar =
            div()
                .flex_none()
                .h(px(44.))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .bg(color("popover"))
                .border_b_1()
                .border_color(color("border"))
                .child(button("browser-back", "‹", "browser.back"))
                .child(button("browser-forward", "›", "browser.forward"))
                .child(button("browser-reload", "↻", "browser.reload"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .px(px(10.))
                        .rounded_md()
                        .bg(color("surface"))
                        .border_1()
                        .border_color(color("border"))
                        .text_size(px(13.))
                        .child(Input::new(&self.input)),
                )
                .child(div().text_size(px(11.)).text_color(color("muted")).child(
                    if self.loading {
                        "Loading…"
                    } else {
                        "Chromium"
                    },
                ));
        #[cfg(target_os = "macos")]
        let host = self.host.clone();
        let allowed = self.allowed.clone();
        let body = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .bg(color("background"))
            .child(
                canvas(
                    move |bounds, _, _| {
                        #[cfg(target_os = "macos")]
                        if let Some(host) = &host {
                            host.place(bounds, allowed.get());
                        }
                    },
                    |_, _, _, _| {},
                )
                .size_full(),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(color("foreground"))
            .child(toolbar)
            .when_some(self.notice.clone(), |el, message| {
                el.child(
                    div()
                        .px(px(10.))
                        .py(px(4.))
                        .text_size(px(12.))
                        .child(message),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .p(px(10.))
                        .bg(color("popover"))
                        .text_color(color("blocked"))
                        .text_size(px(12.))
                        .child(error),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::address;
    #[test]
    fn addresses_and_searches() {
        assert_eq!(
            address("localhost:3000/test").unwrap(),
            "http://localhost:3000/test"
        );
        assert_eq!(address("example.com").unwrap(), "https://example.com/");
        assert!(
            address("some search terms")
                .unwrap()
                .starts_with("https://duckduckgo.com/?q=")
        );
        assert_eq!(address("").unwrap(), "about:blank");
    }
    #[test]
    fn does_not_turn_privileged_schemes_into_page_navigation() {
        for input in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
            "about:config",
        ] {
            assert!(address(input).is_err(), "{input}");
        }
    }
}
