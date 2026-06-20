//! In-app assistant chat on the detail views.
//!
//! Opens a chat dialog (from the detail dialog's "Assistant" row) scoped to the
//! shown object — an artist, album, … . The transcript lives in
//! [`AssistantState`](crate::ui::app::AssistantState) on the `App`, not in the
//! dialog, so it survives the detail dialog's rebuilds. A user turn runs the
//! agent loop ([`crate::core::assistant::agent`]) on a background command thread;
//! its reply comes back as [`Cmd::AssistantReplied`] and re-renders the list.
//!
//! Destructive tools are declined for now (`deny_destructive`); an interactive
//! confirmation is a follow-up.

use std::sync::Arc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::assistant::agent::{deny_destructive, Agent};
use crate::core::assistant::llm::{LlmClient, Message};
use crate::core::assistant::MINIMAX_BASE_URL;
use crate::core::mcp::{ControlFn, McpContext};
use crate::i18n::gettext;
use crate::ui::app::{App, AssistantUi, Cmd, CtxTarget, Msg};

/// In-app assistant messages, dispatched by [`App::update_assistant`]. Grouped
/// out of the flat `Msg` enum (see `app.rs`): the provider/model/key settings
/// plus opening the chat and sending a turn.
#[derive(Debug)]
pub(crate) enum AssistantMsg {
    /// Pick the assistant LLM provider preset (`minimax` / `custom`).
    SetProvider(String),
    /// Persist the assistant API base URL (custom provider only).
    SetBaseUrl(String),
    /// Persist the assistant model name.
    SetModel(String),
    /// Persist the assistant API key (Secret Service).
    SetApiKey(String),
    /// Open the assistant chat for the currently shown detail object.
    OpenChat,
    /// Send a user message in the open chat → run the agent.
    Send(String),
}

impl App {
    /// Dispatch an [`AssistantMsg`]. Split out of the monolithic `App::update`.
    pub(crate) fn update_assistant(
        &mut self,
        msg: AssistantMsg,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match msg {
            AssistantMsg::SetProvider(provider) => {
                let _ = self.library.set_setting("assistant_provider", &provider);
                // MiniMax has a fixed endpoint: prefill it so the user needn't know it.
                if provider == "minimax" {
                    let _ = self
                        .library
                        .set_setting("assistant_base_url", MINIMAX_BASE_URL);
                }
            }
            AssistantMsg::SetBaseUrl(url) => {
                let _ = self.library.set_setting("assistant_base_url", url.trim());
            }
            AssistantMsg::SetModel(model) => {
                let _ = self.library.set_setting("assistant_model", model.trim());
            }
            AssistantMsg::SetApiKey(key) => {
                let _ = self
                    .library
                    .set_secret_setting("assistant_api_key", key.trim());
            }
            AssistantMsg::OpenChat => self.open_assistant_chat(root, sender),
            AssistantMsg::Send(text) => self.assistant_send(sender, text),
        }
    }

    /// Opens (or re-opens) the assistant chat for the current detail object.
    pub(crate) fn open_assistant_chat(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(target) = self.nav.context_target.as_ref() else {
            return;
        };
        let subject = target.heading().to_string();
        let system_prompt = assistant_system_prompt(target, &subject);

        // A different subject starts a fresh transcript; the same one keeps it.
        if self.assistant.subject.as_deref() != Some(subject.as_str()) {
            self.assistant.history = vec![Message::system(system_prompt)];
            self.assistant.subject = Some(subject);
        }
        self.assistant.busy = false;

        let dialog = adw::Dialog::builder().title(gettext("Assistant")).build();
        dialog.set_content_width(560);
        dialog.set_content_height(620);
        self.adapt_detail_dialog(&dialog);

        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let msg_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&msg_box)
            .build();
        outer.append(&scroller);

        // Input row at the bottom (outside the scroller, so it never scrolls away
        // and keeps focus across re-renders of the message list).
        let entry = adw::EntryRow::builder()
            .title(gettext("Ask about this…"))
            .build();
        entry.set_show_apply_button(true);
        {
            let sender = sender.clone();
            entry.connect_apply(move |e| {
                let text = e.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                e.set_text("");
                sender.input(Msg::Assistant(AssistantMsg::Send(text)));
            });
        }
        let input_group = adw::PreferencesGroup::builder()
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        input_group.add(&entry);
        outer.append(&input_group);

        dialog.set_child(Some(&outer));
        self.assistant
            .ui
            .replace(Some(AssistantUi { msg_box, scroller }));
        self.assistant_render();
        dialog.present(Some(root));
    }

    /// Handles a user message: append it, kick off the agent in the background.
    pub(crate) fn assistant_send(&mut self, sender: &ComponentSender<Self>, text: String) {
        if self.assistant.busy {
            return;
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.assistant.history.push(Message::user(&text));

        let Some(client) = self.assistant_client() else {
            self.assistant.history.push(Message::assistant(gettext(
                "No assistant model is configured. Set a provider, model and API key in \
                 Settings → Assistant.",
            )));
            self.assistant_render();
            return;
        };

        self.assistant.busy = true;
        self.assistant_render();

        let history = self.assistant.history.clone();
        let ctx = self.assistant_context();
        sender.spawn_oneshot_command(move || {
            let agent = Agent::new(client, ctx, deny_destructive());
            let mut hist = history;
            let error = agent.run(&mut hist).err().map(|e| e.to_string());
            Cmd::AssistantReplied {
                history: hist,
                error,
            }
        });
    }

    /// A background agent run finished: adopt the new transcript, re-render, and
    /// refresh the detail dialog (the agent may have changed library data, e.g.
    /// enriched the artist gallery, through its own DB connection).
    pub(crate) fn on_assistant_replied(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        history: Vec<Message>,
        error: Option<String>,
    ) {
        self.assistant.busy = false;
        self.assistant.history = history;
        if let Some(err) = error {
            self.assistant
                .history
                .push(Message::assistant(format!("{} {err}", gettext("Error:"))));
        }
        self.assistant_render();
        // Reflect any tool-driven library changes on the open detail dialog.
        self.refresh_context_dialog(root, sender);
    }

    /// Builds the LLM client from the saved settings, or `None` when the model
    /// or API key is missing (the chat then shows a hint instead of running).
    fn assistant_client(&self) -> Option<LlmClient> {
        let get = |k: &str| self.library.get_setting(k).ok().flatten();
        let provider = get("assistant_provider").unwrap_or_else(|| "minimax".into());
        let base_url = if provider == "minimax" {
            MINIMAX_BASE_URL.to_string()
        } else {
            get("assistant_base_url").unwrap_or_default()
        };
        let model = get("assistant_model").unwrap_or_default();
        let key = self
            .library
            .get_secret_setting("assistant_api_key")
            .ok()
            .flatten()
            .unwrap_or_default();
        if base_url.is_empty() || model.trim().is_empty() || key.trim().is_empty() {
            return None;
        }
        Some(LlmClient::new(base_url, key, model))
    }

    /// The in-process tool context for an agent run: reads the live now-playing
    /// snapshot, routes actions back into the UI as `Msg::Mcp`, shares the job
    /// registry — exactly like the MCP server's context.
    fn assistant_context(&self) -> Arc<McpContext> {
        let input = self.input.clone();
        let control: ControlFn = Arc::new(move |cmd| {
            let _ = input.send(Msg::Mcp(cmd));
        });
        Arc::new(McpContext {
            now: self.mcp.now.clone(),
            control,
            jobs: self.mcp.jobs.clone(),
        })
    }

    /// Rebuilds the message list from the transcript (cheap — chats are short),
    /// then pins the view to the newest message. Only user + assistant-text turns
    /// are shown; tool calls/results stay internal to the model.
    fn assistant_render(&self) {
        let ui = self.assistant.ui.borrow();
        let Some(ui) = ui.as_ref() else {
            return;
        };
        while let Some(child) = ui.msg_box.first_child() {
            ui.msg_box.remove(&child);
        }
        for m in &self.assistant.history {
            if let Some(bubble) = assistant_bubble(m) {
                ui.msg_box.append(&bubble);
            }
        }
        if self.assistant.busy {
            ui.msg_box.append(&thinking_row());
        }
        // Scroll to the bottom once the new children have been laid out.
        let adj = ui.scroller.vadjustment();
        gtk::glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }
}

/// The system prompt describing the object the chat is about.
fn assistant_system_prompt(target: &CtxTarget, subject: &str) -> String {
    let detail = match target {
        CtxTarget::Artist(m) => format!("the artist \"{}\"", m.name),
        CtxTarget::Album(m) => format!("the album \"{}\" by \"{}\"", m.album, m.artist),
        CtxTarget::Fs(_) => format!("\"{subject}\""),
    };
    format!(
        "You are an assistant embedded in the Emilia music player. The user is viewing the \
         detail page of {detail}. Help them with tasks about it, using the available tools \
         (search, playback, playlists, artist images, …). Prefer acting over describing when \
         the user asks you to do something. Be concise, and state what you changed."
    )
}

/// A chat bubble for a visible turn, or `None` for internal (tool) turns.
fn assistant_bubble(m: &Message) -> Option<gtk::Widget> {
    let (who, text) = match m.role.as_str() {
        "user" => (gettext("You"), m.content.clone()?),
        "assistant" => (
            gettext("Assistant"),
            m.content.clone().filter(|s| !s.trim().is_empty())?,
        ),
        _ => return None,
    };
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    let who_label = gtk::Label::builder().label(&who).xalign(0.0).build();
    who_label.add_css_class("caption");
    who_label.add_css_class("dim-label");
    let body = gtk::Label::builder()
        .label(&text)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .build();
    body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    row.append(&who_label);
    row.append(&body);
    Some(row.upcast())
}

/// The "Thinking…" placeholder shown while an agent run is in flight.
fn thinking_row() -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    let label = gtk::Label::builder()
        .label(gettext("Thinking…"))
        .xalign(0.0)
        .build();
    label.add_css_class("dim-label");
    row.append(&spinner);
    row.append(&label);
    row.upcast()
}
