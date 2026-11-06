//! The MCP servers section of the project settings dialog.
//!
//! Rendering only reads the model and edits the in-memory draft; every load
//! and save is a returned [`McpSettingsAction`] the model runs off-thread.

use egui::{RichText, Ui};

use config::mcp::{MCP_ROLE_KEYS, McpServerEntry, McpTransportDefinition};

use super::draft::{
    DraftCheck, DraftField, DraftPair, DraftTransport, DraftValue, DraftValueKind, McpServerDraft,
    looks_secret,
};
use super::{
    EditorParts, McpEntryActions, McpEntryOrigin, McpLoadState, McpNotice, McpRowNote,
    McpSettingsAction, McpSettingsModel,
};
use crate::mcp::McpServerStatus;
use crate::theme::icons;
use crate::theme::text::{h3, h4, muted, semibold};
use crate::theme::tokens::*;
use crate::theme::widgets::{
    accessible, badge, primary_button, soft_frame, spinner, surface_frame,
};

pub const MCP_SECTION_LABEL: &str = "MCP servers";
pub const MCP_SERVERS_BUTTON_LABEL: &str = "MCP servers…";
pub const CLOSE_MCP_LABEL: &str = "Close MCP servers";
pub const ADD_SERVER_LABEL: &str = "Add MCP server";
pub const SAVE_SERVER_LABEL: &str = "Save server";
pub const APPLY_NOTICE: &str = "Saved changes apply from the next new run. Running conversations keep their tools. Opening or saving this list never starts or connects to a server.";
const IMPORTED_NOTE: &str = "Defined in .mcp.json, which evorch never modifies. Saving writes a complete override to .evorch/mcp.toml.";
const SECRET_HINT: &str = "Values are saved as typed in .evorch/mcp.toml. For secrets, reference an environment variable as ${NAME} (or ${NAME:-default}), or a keychain entry by name.";

/// The MCP servers dialog, stacked over the project settings.
pub fn mcp_settings_modal(
    ctx: &egui::Context,
    model: &mut McpSettingsModel,
) -> Option<McpSettingsAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("mcp-settings-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.7).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 2.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3(MCP_SECTION_LABEL));
            egui::ScrollArea::vertical()
                .max_height(ctx.viewport_rect().height() * 0.7)
                .auto_shrink([false, true])
                .show(ui, |ui| action = mcp_settings_section(ui, model));
            ui.add_space(SP_1);
            let close = ui.button("Close");
            accessible(&close, CLOSE_MCP_LABEL);
            if close.clicked() {
                action = Some(McpSettingsAction::Close);
            }
        });
    // A stray click or Escape must not throw away an unsaved edit, and Escape
    // in the approval dialog only closes that dialog.
    let approval = super::review::approval_modal(ctx, model);
    if approval.is_some() {
        return approval;
    }
    if action.is_none()
        && modal.should_close()
        && model.editor().is_none()
        && model.approval().is_none()
    {
        action = Some(McpSettingsAction::Close);
    }
    action
}

/// Draws the section and returns what the user asked for. Only the open
/// draft is edited in place; loads and saves are left to the caller.
pub fn mcp_settings_section(
    ui: &mut Ui,
    model: &mut McpSettingsModel,
) -> Option<McpSettingsAction> {
    let listed = server_list(ui, model);
    let edited = mcp_editor(ui, model);
    edited.or(listed)
}

fn server_list(ui: &mut Ui, model: &McpSettingsModel) -> Option<McpSettingsAction> {
    let mut action = None;
    ui.label(muted(APPLY_NOTICE));
    match model.load_state() {
        None => return None,
        Some(McpLoadState::Loading) => {
            ui.horizontal(|ui| {
                spinner(ui);
                ui.label(muted("Loading MCP servers…"));
            });
            return None;
        }
        Some(McpLoadState::Failed(message)) => {
            ui.colored_label(palette().ERROR_FG, message);
            if model.is_available() && ui.button("Retry").clicked() {
                action = Some(McpSettingsAction::Reload);
            }
            return action;
        }
        Some(McpLoadState::Loaded) => {}
    }
    let config = model.config()?;
    let busy = model.is_busy();
    notice(ui, model.notice(), busy, &mut action);
    if config.preparation_blocked {
        ui.colored_label(
            palette().WARNING_FG,
            ".evorch/mcp.toml could not be read. No MCP server starts for new runs until it is fixed; .mcp.json is not used as a fallback.",
        );
    }
    for diagnostic in &config.diagnostics {
        ui.colored_label(
            palette().ERROR_FG,
            format!(
                "{}: {}",
                source_file(diagnostic.source),
                super::config_error_text(diagnostic.code)
            ),
        );
    }
    if config.servers.is_empty() && !config.preparation_blocked {
        ui.label(muted(
            "No MCP servers. Add one here or in .mcp.json at the project root.",
        ));
    }
    if model.editor().is_some() {
        // The editor replaces the list so the form reads top to bottom.
        return action;
    }
    for (index, (id, entry)) in config.servers.iter().enumerate() {
        if index > 0 {
            ui.separator();
        }
        ui.push_id(("mcp-server", id), |ui| {
            server_row(ui, model, id, entry, busy, &mut action);
        });
    }
    let add = ui.add_enabled(
        !busy && !config.preparation_blocked,
        egui::Button::new(icons::with_icon(icons::PLUS, ADD_SERVER_LABEL)),
    );
    accessible(&add, ADD_SERVER_LABEL);
    if add.clicked() {
        action = Some(McpSettingsAction::Add);
    }
    action
}

fn mcp_editor(ui: &mut Ui, model: &mut McpSettingsModel) -> Option<McpSettingsAction> {
    let check = model.check_editor()?;
    let busy = model.is_busy();
    let parts = model.editor_parts()?;
    let mut action = None;
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
        editor_body(ui, parts, &check, busy, &mut action);
    });
    action
}

fn notice(
    ui: &mut Ui,
    notice: Option<&McpNotice>,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    match notice {
        None => {}
        Some(McpNotice::Saved) => {
            ui.colored_label(
                palette().SUCCESS,
                "Saved. It applies from the next new run.",
            );
        }
        Some(McpNotice::SecretStored) => {
            ui.colored_label(
                palette().SUCCESS,
                "Saved. The secret is in the OS keychain; .evorch/mcp.toml holds only its entry name. It applies from the next new run.",
            );
        }
        Some(McpNotice::Conflict) => {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    palette().WARNING_FG,
                    "The MCP configuration changed on disk since it was loaded, so nothing was saved. Reload to see the current files, then save again.",
                );
                if ui.add_enabled(!busy, egui::Button::new("Reload")).clicked() {
                    *action = Some(McpSettingsAction::Reload);
                }
            });
        }
        Some(McpNotice::Failed(message)) => {
            ui.colored_label(palette().ERROR_FG, message);
        }
    }
}

fn source_file(source: config::mcp::McpSource) -> &'static str {
    match source {
        config::mcp::McpSource::ImportedJson => ".mcp.json",
        config::mcp::McpSource::ProjectToml => ".evorch/mcp.toml",
    }
}

fn server_row(
    ui: &mut Ui,
    model: &McpSettingsModel,
    id: &str,
    entry: &McpServerEntry,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    let origin = McpEntryOrigin::of(entry);
    ui.horizontal_wrapped(|ui| {
        ui.label(semibold(id).color(palette().TEXT));
        badge(ui, origin.label(), palette().TEXT_MUTED, palette().SURFACE);
        if let Some(definition) = &entry.definition {
            let transport = match definition.transport {
                McpTransportDefinition::Stdio { .. } => "stdio",
                McpTransportDefinition::Http { .. } => "http",
            };
            badge(ui, transport, palette().TEXT_MUTED, palette().SURFACE);
            if !definition.enabled {
                badge(ui, "disabled", palette().WARNING, palette().SURFACE);
            }
        } else if entry.tombstone {
            badge(ui, "disabled", palette().WARNING, palette().SURFACE);
        } else {
            badge(ui, "invalid", palette().ERROR_FG, palette().SURFACE);
        }
        status_slot(ui, model, id);
    });
    if let Some(definition) = &entry.definition {
        let roles = if definition.roles.is_empty() {
            "Roles: none (not published)".to_owned()
        } else {
            format!("Roles: {}", definition.roles.join(", "))
        };
        ui.label(muted(roles));
    } else if entry.tombstone && entry.imported_exists {
        ui.label(muted(
            "Hidden by mcp.toml; the .mcp.json definition is not used.",
        ));
    } else if entry.tombstone {
        ui.label(muted(
            "mcp.toml disables this ID, but no .mcp.json server uses it.",
        ));
    }
    if !entry.tombstone {
        super::review::status_controls(ui, model, id, entry, busy, action);
    }
    for diagnostic in &entry.diagnostics {
        ui.colored_label(
            palette().ERROR_FG,
            format!(
                "{}: {}",
                source_file(diagnostic.source),
                super::config_error_text(diagnostic.code)
            ),
        );
    }
    if model.pending_delete() == Some(id) {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                palette().WARNING_FG,
                format!("Delete {id} from .evorch/mcp.toml?"),
            );
            let confirm = ui.add_enabled(!busy, egui::Button::new("Delete"));
            accessible(&confirm, &format!("Confirm delete {id}"));
            if confirm.clicked() {
                *action = Some(McpSettingsAction::ConfirmDelete);
            }
            if ui.button("Cancel").clicked() {
                *action = Some(McpSettingsAction::CancelDelete);
            }
        });
        return;
    }
    let actions = McpEntryActions::of(entry);
    let enabled = !busy;
    ui.horizontal_wrapped(|ui| {
        let mut row_button = |ui: &mut Ui, text: &str, choice: McpSettingsAction| {
            let response = ui.add_enabled(enabled, egui::Button::new(text).small());
            accessible(&response, &format!("{text} {id}"));
            if response.clicked() {
                *action = Some(choice);
            }
        };
        if actions.edit {
            row_button(ui, "Edit", McpSettingsAction::Edit(id.to_owned()));
        }
        if let Some(target) = actions.set_enabled {
            let text = if target { "Enable" } else { "Disable" };
            row_button(
                ui,
                text,
                McpSettingsAction::SetEnabled {
                    server_id: id.to_owned(),
                    enabled: target,
                },
            );
        }
        if actions.disable {
            let text = if origin == McpEntryOrigin::ImportedJson {
                "Disable"
            } else {
                "Hide import"
            };
            row_button(ui, text, McpSettingsAction::Disable(id.to_owned()));
        }
        if actions.remove_override {
            row_button(
                ui,
                "Remove override",
                McpSettingsAction::RemoveOverride(id.to_owned()),
            );
        }
        if actions.delete {
            row_button(
                ui,
                "Delete",
                McpSettingsAction::RequestDelete(id.to_owned()),
            );
        }
    });
    ui.add_space(SP_1);
}

/// Live state from the service. An approval or revocation made in this
/// dialog takes precedence until the dialog is closed.
fn status_slot(ui: &mut Ui, model: &McpSettingsModel, id: &str) {
    let (text, color) = match model.row_note(id) {
        Some(McpRowNote::Approved) => ("approved", palette().SUCCESS),
        Some(McpRowNote::Revoked) => super::review::status_badge(McpServerStatus::NeedsApproval),
        _ => match model.status(id) {
            Some(event) => super::review::status_badge(event.status),
            None => return,
        },
    };
    badge(ui, text, color, palette().SURFACE);
}

fn editor_body(
    ui: &mut Ui,
    parts: EditorParts<'_>,
    check: &DraftCheck,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    let draft = &mut *parts.draft;
    let title = match &draft.existing_id {
        Some(id) => format!("Edit {id}"),
        None => "New MCP server".to_owned(),
    };
    ui.label(h4(title));
    if draft.imported {
        ui.label(muted(IMPORTED_NOTE));
    }
    if draft.is_new() {
        text_field(
            ui,
            "Server name",
            &mut draft.id,
            "files",
            check.error(DraftField::Id),
        );
    }
    ui.horizontal(|ui| {
        ui.label("Transport");
        ui.radio_value(
            &mut draft.transport,
            DraftTransport::Stdio,
            "Local command (stdio)",
        );
        ui.radio_value(&mut draft.transport, DraftTransport::Http, "HTTP");
    });
    ui.checkbox(&mut draft.enabled, "Enabled");
    ui.label(muted("Agent roles that get this server's tools"));
    ui.horizontal_wrapped(|ui| {
        for role in MCP_ROLE_KEYS {
            let mut on = draft.roles.contains(*role);
            if ui.checkbox(&mut on, *role).changed() {
                if on {
                    draft.roles.insert((*role).to_owned());
                } else {
                    draft.roles.remove(*role);
                }
            }
        }
    });
    match draft.transport {
        DraftTransport::Stdio => stdio_fields(ui, draft, check),
        DraftTransport::Http => http_fields(ui, draft, check),
    }
    egui::CollapsingHeader::new("Timeouts")
        .id_salt("mcp-timeouts")
        .show(ui, |ui| {
            text_field(
                ui,
                "Startup timeout (ms)",
                &mut draft.startup_timeout_ms,
                "",
                check.error(DraftField::StartupTimeout),
            );
            text_field(
                ui,
                "Request timeout (ms)",
                &mut draft.request_timeout_ms,
                "",
                check.error(DraftField::RequestTimeout),
            );
        });
    ui.label(muted(SECRET_HINT));
    super::review::secret_section(ui, parts, check, busy, action);
    for warning in &check.warnings {
        ui.colored_label(palette().WARNING_FG, warning.message());
    }
    if let Some(error) = check.error(DraftField::Server) {
        ui.colored_label(palette().ERROR_FG, error);
    }
    ui.horizontal(|ui| {
        let can_save = check.is_valid() && !busy;
        let save = ui
            .add_enabled_ui(can_save, |ui| primary_button(ui, SAVE_SERVER_LABEL))
            .inner;
        accessible(&save, SAVE_SERVER_LABEL);
        if save.clicked() && can_save {
            *action = Some(McpSettingsAction::SaveEdit);
        }
        if ui.button("Cancel").clicked() {
            *action = Some(McpSettingsAction::CancelEdit);
        }
        if busy {
            spinner(ui);
        }
    });
}

fn stdio_fields(ui: &mut Ui, draft: &mut McpServerDraft, check: &DraftCheck) {
    text_field(
        ui,
        "Command",
        &mut draft.command,
        "node",
        check.error(DraftField::Command),
    );
    ui.label(muted("Arguments (one per row, passed without a shell)"));
    let mut remove = None;
    for (index, arg) in draft.args.iter_mut().enumerate() {
        ui.push_id(("arg", index), |ui| {
            ui.horizontal(|ui| {
                if remove_button(ui, &format!("Remove argument {}", index + 1)) {
                    remove = Some(index);
                }
                single_line(ui, arg, &format!("Argument {}", index + 1), "");
            });
            field_error(ui, check.error(DraftField::Arg(index)));
        });
    }
    if let Some(index) = remove {
        draft.args.remove(index);
    }
    if add_button(ui, "Add argument") {
        draft.args.push(String::new());
    }
    text_field(
        ui,
        "Working directory (relative to the project root)",
        &mut draft.cwd,
        ".",
        check.error(DraftField::Cwd),
    );
    ui.label(muted("Environment variables"));
    pair_rows(
        ui,
        &mut draft.env,
        check,
        "Variable",
        DraftField::EnvName,
        DraftField::EnvValue,
    );
    if add_button(ui, "Add variable") {
        draft.env.push(DraftPair::empty());
    }
}

fn http_fields(ui: &mut Ui, draft: &mut McpServerDraft, check: &DraftCheck) {
    text_field(
        ui,
        "URL",
        &mut draft.url,
        "https://mcp.example.com/mcp",
        check.error(DraftField::Url),
    );
    ui.label(muted("Bearer token"));
    let mut kind = draft.token.as_ref().map(|token| token.kind);
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("mcp-token-kind")
            .selected_text(match kind {
                None => "None",
                Some(DraftValueKind::Text) => "Text / ${VAR}",
                Some(DraftValueKind::Keychain) => "Keychain",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut kind, None, "None");
                ui.selectable_value(&mut kind, Some(DraftValueKind::Text), "Text / ${VAR}");
                ui.selectable_value(&mut kind, Some(DraftValueKind::Keychain), "Keychain");
            });
        match (kind, &mut draft.token) {
            (None, token) => *token = None,
            (Some(kind), Some(token)) => {
                token.kind = kind;
                if kind == DraftValueKind::Text {
                    masked_line(ui, &mut token.text, "Bearer token value", value_hint(kind));
                } else {
                    single_line(ui, &mut token.text, "Bearer token value", value_hint(kind));
                }
            }
            (Some(kind), token @ None) => {
                *token = Some(DraftValue {
                    kind,
                    text: String::new(),
                });
            }
        }
    });
    field_error(ui, check.error(DraftField::Token));
    ui.label(muted("Headers"));
    pair_rows(
        ui,
        &mut draft.headers,
        check,
        "Header",
        DraftField::HeaderName,
        DraftField::HeaderValue,
    );
    if add_button(ui, "Add header") {
        draft.headers.push(DraftPair::empty());
    }
}

fn pair_rows(
    ui: &mut Ui,
    pairs: &mut Vec<DraftPair>,
    check: &DraftCheck,
    noun: &str,
    name_field: fn(usize) -> DraftField,
    value_field: fn(usize) -> DraftField,
) {
    let mut remove = None;
    for (index, pair) in pairs.iter_mut().enumerate() {
        ui.push_id((noun, index), |ui| {
            ui.horizontal(|ui| {
                if remove_button(ui, &format!("Remove {noun} {}", index + 1)) {
                    remove = Some(index);
                }
                let name_width = (ui.available_width() * 0.3).max(80.0);
                let name = ui.add(
                    egui::TextEdit::singleline(&mut pair.name)
                        .hint_text("NAME")
                        .desired_width(name_width),
                );
                let label = format!("{noun} {} name", index + 1);
                name.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, &label)
                });
                egui::ComboBox::from_id_salt("kind")
                    .width(88.0)
                    .selected_text(match pair.value.kind {
                        DraftValueKind::Text => "Text",
                        DraftValueKind::Keychain => "Keychain",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut pair.value.kind, DraftValueKind::Text, "Text");
                        ui.selectable_value(
                            &mut pair.value.kind,
                            DraftValueKind::Keychain,
                            "Keychain",
                        );
                    });
                let label = format!("{noun} {} value", index + 1);
                let hint = value_hint(pair.value.kind);
                // Plain text under a credential-like name may be the secret itself.
                if pair.value.kind == DraftValueKind::Text && looks_secret(&pair.name) {
                    masked_line(ui, &mut pair.value.text, &label, hint);
                } else {
                    single_line(ui, &mut pair.value.text, &label, hint);
                }
            });
            field_error(ui, check.error(name_field(index)));
            field_error(ui, check.error(value_field(index)));
        });
    }
    if let Some(index) = remove {
        pairs.remove(index);
    }
}

fn value_hint(kind: DraftValueKind) -> &'static str {
    match kind {
        DraftValueKind::Text => "value or ${VAR}",
        DraftValueKind::Keychain => "keychain entry name",
    }
}

fn text_field(ui: &mut Ui, label: &str, value: &mut String, hint: &str, error: Option<&str>) {
    ui.label(muted(label));
    single_line(ui, value, label, hint);
    field_error(ui, error);
}

fn single_line(ui: &mut Ui, value: &mut String, label: &str, hint: &str) {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(ui.available_width()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
}

/// A token typed as text may be the credential itself: never echo it.
fn masked_line(ui: &mut Ui, value: &mut String, label: &str, hint: &str) {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .password(true)
            .hint_text(hint)
            .desired_width(ui.available_width()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
}

fn field_error(ui: &mut Ui, error: Option<&str>) {
    if let Some(error) = error {
        ui.label(
            RichText::new(error)
                .size(FONT_SMALL)
                .color(palette().ERROR_FG),
        );
    }
}

fn remove_button(ui: &mut Ui, label: &str) -> bool {
    crate::theme::widgets::icon_button(ui, icons::X, label).clicked()
}

fn add_button(ui: &mut Ui, label: &str) -> bool {
    let response = ui.add(egui::Button::new(icons::with_icon(icons::PLUS, label)).small());
    accessible(&response, label);
    response.clicked()
}
