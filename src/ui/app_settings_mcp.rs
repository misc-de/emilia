//! "MCP server" group of the settings View page: backend, LAN exposure and
//! the bearer token. Split out of [`crate::ui::app_settings`] – pure
//! reorganization, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
use crate::ui::app::{App, Msg};
use crate::ui::app_mcp::McpSettingMsg;
use crate::ui::app_settings::{combo_index, combo_item};

impl App {
    /// Builds the "MCP server" settings group.
    pub(super) fn build_mcp_group(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        let mcp_group = adw::PreferencesGroup::builder()
            .title(gettext("MCP server"))
            .description(gettext(
                "Expose the library and playback to an MCP host (e.g. Claude). \
                 Endpoint: POST http://127.0.0.1:8770/mcp with the bearer token below.",
            ))
            .build();

        // Only the backends this build carries (the SDK one is a cargo feature).
        let mcp_modes: Vec<crate::core::mcp::McpMode> =
            crate::core::mcp::McpMode::selectable().to_vec();
        let mcp_mode_labels: Vec<String> = mcp_modes
            .iter()
            .map(|m| match m {
                crate::core::mcp::McpMode::Off => gettext("Off"),
                crate::core::mcp::McpMode::JsonRpc => gettext("JSON-RPC 2.0 (lean)"),
                crate::core::mcp::McpMode::Sdk => gettext("Tokio SDK"),
            })
            .collect();
        let mcp_mode_refs: Vec<&str> = mcp_mode_labels.iter().map(String::as_str).collect();
        let mcp_mode_row = adw::ComboRow::builder()
            .title(gettext("Backend"))
            .model(&gtk::StringList::new(&mcp_mode_refs))
            .build();
        let cur_mode = self
            .library
            .get_setting("mcp_mode")
            .ok()
            .flatten()
            .map(|s| crate::core::mcp::McpMode::from_setting(&s))
            .unwrap_or(crate::core::mcp::McpMode::Off);
        mcp_mode_row.set_selected(combo_index(&mcp_modes, &cur_mode));
        mcp_group.add(&mcp_mode_row);

        let mcp_public_row = adw::SwitchRow::builder()
            .title(gettext("Public (reachable on the LAN)"))
            .subtitle(gettext(
                "Off: only this device (127.0.0.1). On: the whole network, over TLS.",
            ))
            .build();
        mcp_public_row.set_active(
            self.library
                .get_setting("mcp_public")
                .ok()
                .flatten()
                .as_deref()
                == Some("1"),
        );
        {
            let sender = sender.clone();
            mcp_public_row.connect_active_notify(move |r| {
                sender.input(Msg::McpSetting(McpSettingMsg::SetPublic(r.is_active())));
            });
        }
        mcp_group.add(&mcp_public_row);

        // Bearer token (read-only) with a copy button — generated on first enable.
        let token = self
            .library
            .get_secret_setting("mcp_token")
            .ok()
            .flatten()
            .unwrap_or_default();
        let token_sub = if token.is_empty() {
            gettext("Generated when the server is first enabled")
        } else {
            token.clone()
        };
        let token_row = adw::ActionRow::builder()
            .title(gettext("Bearer token"))
            .subtitle(&token_sub)
            .build();
        token_row.set_subtitle_selectable(true);
        if !token.is_empty() {
            // Shared current token: the copy button always copies the latest, and
            // the regenerate button replaces it in place.
            let token_cell = std::rc::Rc::new(std::cell::RefCell::new(token.clone()));

            let copy_btn = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Copy token"))
                .build();
            copy_btn.add_css_class("flat");
            {
                let token_cell = token_cell.clone();
                copy_btn.connect_clicked(move |b| {
                    b.clipboard().set_text(&token_cell.borrow());
                });
            }

            // Regenerate: confirm first (existing connections break), then create
            // and store a new token and restart the server so the old one dies.
            let refresh_btn = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Generate a new token"))
                .build();
            refresh_btn.add_css_class("flat");
            {
                let sender = sender.clone();
                let root = root.clone();
                let token_cell = token_cell.clone();
                let token_row = token_row.clone();
                refresh_btn.connect_clicked(move |_| {
                    let confirm = adw::AlertDialog::new(
                        Some(&gettext("Generate a new token?")),
                        Some(&gettext(
                            "A new bearer token is created immediately. Existing \
                             connections stop working, and every MCP host must be \
                             updated with the new token.",
                        )),
                    );
                    confirm.add_response("cancel", &gettext("Cancel"));
                    confirm.add_response("regenerate", &gettext("Generate new token"));
                    confirm.set_response_appearance(
                        "regenerate",
                        adw::ResponseAppearance::Destructive,
                    );
                    confirm.set_default_response(Some("cancel"));
                    confirm.set_close_response("cancel");
                    let sender = sender.clone();
                    let token_cell = token_cell.clone();
                    let token_row = token_row.clone();
                    confirm.connect_response(None, move |_, resp| {
                        if resp == "regenerate" {
                            let new = crate::core::sync::crypto::generate_token(32);
                            token_cell.replace(new.clone());
                            token_row.set_subtitle(&new);
                            sender.input(Msg::McpSetting(McpSettingMsg::SetToken(new)));
                        }
                    });
                    confirm.present(Some(&root));
                });
            }

            token_row.add_suffix(&copy_btn);
            token_row.add_suffix(&refresh_btn);
        }
        mcp_group.add(&token_row);

        // The public switch + token only matter once a backend is selected: hide
        // them while the server is "Off", and toggle them live with the combo.
        let mcp_enabled = cur_mode != crate::core::mcp::McpMode::Off;
        mcp_public_row.set_visible(mcp_enabled);
        token_row.set_visible(mcp_enabled);
        {
            // Connect after `set_selected`, so the preselection doesn't fire.
            let sender = sender.clone();
            let public_row = mcp_public_row.clone();
            let token_row = token_row.clone();
            mcp_mode_row.connect_selected_notify(move |r| {
                let mode = combo_item(&mcp_modes, r.selected(), crate::core::mcp::McpMode::Off);
                let on = mode != crate::core::mcp::McpMode::Off;
                public_row.set_visible(on);
                token_row.set_visible(on);
                sender.input(Msg::McpSetting(McpSettingMsg::SetMode(mode)));
            });
        }
        mcp_group
    }
}
