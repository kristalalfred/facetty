use anyhow::Result;
use facetty_proto::ParticipantId;
use wasm_bindgen::JsCast;
use web_sys::{HtmlAnchorElement, HtmlElement, HtmlInputElement};

use crate::util::{create, document, element};

/// Claude: how close to the bottom, in pixels, still counts as reading the
/// newest messages.
const FOLLOW: f64 = 48.0;

pub struct Chat {
    panel: HtmlElement,
    list: HtmlElement,
    input: HtmlInputElement,
    last_from: Option<ParticipantId>,
}

impl Chat {
    pub fn new() -> Result<Self> {
        Ok(Self {
            panel: element("chat")?,
            list: element("messages")?,
            input: element("say")?,
            last_from: None,
        })
    }

    pub fn input(&self) -> &HtmlInputElement {
        &self.input
    }

    pub fn set_open(&self, open: bool) {
        self.panel.set_hidden(!open);
        if open {
            self.list.set_scroll_top(self.list.scroll_height() as f64);
        } else {
            let _ = self.input.blur();
        }
    }

    pub fn width(&self) -> f64 {
        self.panel.offset_width() as f64
    }

    pub fn focus(&self) {
        let _ = self.input.focus();
    }

    pub fn push(&mut self, from: ParticipantId, name: &str, text: &str, mine: bool) {
        let list = &self.list;
        let below =
            list.scroll_height() as f64 - list.scroll_top() - f64::from(list.client_height());
        let following = below < FOLLOW;
        let item = create("li", if mine { "mine" } else { "" });
        if self.last_from != Some(from) {
            let who = create("span", "from");
            who.set_text_content(Some(name));
            let _ = item.append_child(&who);
        }
        self.last_from = Some(from);
        let body = create("p", "");
        let mut done = 0;
        for link in facetty_proto::chat::links(text) {
            append_text(&body, &text[done..link.start]);
            let url = &text[link.clone()];
            let a: HtmlAnchorElement = create("a", "").unchecked_into();
            a.set_href(url);
            a.set_target("_blank");
            a.set_rel("noopener noreferrer");
            a.set_text_content(Some(url));
            let _ = body.append_child(&a);
            done = link.end;
        }
        append_text(&body, &text[done..]);
        let _ = item.append_child(&body);
        let _ = self.list.append_child(&item);
        if following || mine {
            self.list.set_scroll_top(self.list.scroll_height() as f64);
        }
    }
}

fn append_text(parent: &HtmlElement, text: &str) {
    if !text.is_empty() {
        let _ = parent.append_child(&document().create_text_node(text));
    }
}
