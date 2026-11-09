//! Approval review, row status and keychain input of the MCP servers dialog.
//!
//! Everything here only reads the model or edits the open draft; approval,
//! revocation, connection tests and secret storage are returned as actions
//! that the model runs off the render thread.

use std::collections::{BTreeMap, BTreeSet};

use egui::{RichText, Ui};

use config::mcp::{McpServerEntry, McpTransportDefinition, TemplatePart, ValueSource};
use runtime::mcp::{ApprovalWarning, McpApprovalSummary, McpDestinationSummary};

use super::draft::{DraftCheck, looks_secret, template_text};
use super::{
    EditorParts, McpApprovalState, McpRowNote, McpSettingsAction, McpSettingsModel, SecretInput,
};
use crate::mcp::{McpApprovalPreview, McpErrorKind, McpServerStatus, McpStatusEvent};
use crate::theme::text::{h3, h4, muted, semibold};
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, badge, primary_button, spinner, surface_frame};

pub const APPROVE_LABEL: &str = "Approve";
pub const REVIEW_AGAIN_LABEL: &str = "Review again";
pub const STORE_SECRET_LABEL: &str = "Store in keychain and save";
pub const NEXT_RUN_NOTICE: &str = "Approved. It is used from the next new run; conversations already running keep their current tools.";
pub const OUTSIDE_SANDBOX_WARNING: &str = "This starts a program on this computer outside the sandbox. It can run any command with your user permissions.";
pub const UNVERIFIED_CONTENT_NOTE: &str = "Approval allows exactly the settings below. It does not check what the program, its scripts or the server actually do.";
pub const STALE_PREVIEW_TEXT: &str = "The MCP configuration changed after this preview, so nothing was approved. Review the current settings again.";
pub const OTHER_CHANGES_TEXT: &str = "Other approved settings changed, such as the command, arguments, URL, plain values or the network approval (allowed addresses). Their previous values are not kept, so check everything below.";
pub const OTHER_MAYBE_CHANGED_TEXT: &str = "Other approved settings may also have changed, such as the command, arguments, URL, plain values or the network approval (allowed addresses); their previous values are not kept.";
pub const TEST_REFUSED_TEXT: &str =
    "Not tested: approve this server first. Nothing was started or contacted.";
const REVOKED_TEXT: &str = "Approval revoked. Conversations already running keep their tool list, but calls to this server are refused.";
const SECRET_STORAGE_NOTE: &str = "The value goes only to the OS keychain. .evorch/mcp.toml keeps just the entry name, and the value is never shown again. If the keychain cannot store it, nothing is saved.";

/// Short label and color of a reported server state.
pub(super) fn status_badge(status: McpServerStatus) -> (&'static str, egui::Color32) {
    match status {
        McpServerStatus::Disabled => ("off", palette().TEXT_MUTED),
        McpServerStatus::Invalid => ("invalid", palette().ERROR_FG),
        McpServerStatus::NeedsApproval => ("needs approval", palette().WARNING),
        McpServerStatus::SecretUnavailable => ("secret unavailable", palette().WARNING),
        McpServerStatus::Connecting => ("connecting", palette().INFO),
        McpServerStatus::Ready => ("ready", palette().SUCCESS),
        McpServerStatus::Unavailable => ("unavailable", palette().ERROR_FG),
        McpServerStatus::ProcessLimitReached => ("process limit", palette().WARNING),
    }
}

/// One sentence for a reported state, from its safe classification only.
fn status_text(event: &McpStatusEvent) -> Option<String> {
    if let Some(reason) = event.reason {
        return Some(status_reason_text(reason));
    }
    Some(
        match event.status {
            McpServerStatus::NeedsApproval => status_reason_text(McpErrorKind::NeedsApproval),
            McpServerStatus::ProcessLimitReached => {
                status_reason_text(McpErrorKind::ProcessLimitReached)
            }
            McpServerStatus::SecretUnavailable => {
                status_reason_text(McpErrorKind::SecretUnavailable)
            }
            McpServerStatus::Invalid => status_reason_text(McpErrorKind::Configuration),
            _ => return None,
        }
        .to_owned(),
    )
}

/// User-facing text for a classified MCP failure. Never includes URLs,
/// arguments, header or secret values.
pub fn status_reason_text(kind: McpErrorKind) -> String {
    match kind {
        McpErrorKind::NeedsApproval => {
            "Not started: it is not approved. Approve it to use it from the next new run.".into()
        }
        McpErrorKind::ApprovalRevoked => {
            "Approval was revoked; calls to this server are refused.".into()
        }
        McpErrorKind::RoleNotAllowed => {
            "This server is not shared with this role; calls to it are refused.".into()
        }
        McpErrorKind::StalePreview => STALE_PREVIEW_TEXT.into(),
        McpErrorKind::ConfigConflict => {
            "The MCP configuration changed on disk. Reload and try again.".into()
        }
        McpErrorKind::SecretUnavailable
        | McpErrorKind::SecretLocked
        | McpErrorKind::SecretDenied
        | McpErrorKind::SecretMissing => secret_error_text(kind),
        McpErrorKind::MissingEnvironment(field) => {
            format!("An environment variable used in {field} is not set in evorch's environment.")
        }
        McpErrorKind::Configuration => "The server definition is invalid.".into(),
        McpErrorKind::ProcessLimitReached => {
            "Too many local MCP servers are running for this project; it was not started.".into()
        }
        McpErrorKind::PreparationLimitReached => {
            "Too many MCP connections are being prepared for this project; this server is unavailable for this run.".into()
        }
        McpErrorKind::Timeout => "The server did not answer in time.".into(),
        McpErrorKind::Transport => {
            "The server could not be reached or stopped unexpectedly.".into()
        }
        McpErrorKind::HttpStatus(status) => format!("The server answered with HTTP {status}."),
        McpErrorKind::Protocol => "The server's response was not valid MCP.".into(),
        McpErrorKind::ServerRejected => "The server rejected the request.".into(),
        McpErrorKind::ToolRejected => "The tool reported an error.".into(),
        McpErrorKind::Unavailable => "The server is unavailable.".into(),
        McpErrorKind::Cancelled => "Cancelled.".into(),
        McpErrorKind::OutcomeUnknown => {
            "The connection ended before the result arrived; it is not retried.".into()
        }
        McpErrorKind::InputRequired => {
            "The server asked for interactive input, which evorch does not support.".into()
        }
        McpErrorKind::LimitExceeded => "The server's response exceeded a size limit.".into(),
        McpErrorKind::DefinitionChanged => {
            "The server's tool definitions changed since this conversation started.".into()
        }
    }
}

pub fn secret_deleted_text(name: &str) -> String {
    format!(
        "Deleted. The keychain entry \"{name}\" is removed from the OS keychain and from this server's settings."
    )
}

/// The settings no longer reference the entry, but its value may remain.
pub fn secret_delete_partial_text(name: &str, reason: McpErrorKind) -> String {
    let cause = match reason {
        McpErrorKind::SecretUnavailable => "the OS keychain is not available",
        McpErrorKind::SecretLocked => "the OS keychain is locked",
        McpErrorKind::SecretDenied => "access to the OS keychain was denied or cancelled",
        McpErrorKind::ConfigConflict => "the MCP configuration changed on disk meanwhile",
        _ => "the OS keychain reported an error",
    };
    format!(
        "Saved without the reference to \"{name}\", but the keychain entry was not deleted because {cause}. Its value may still be stored in the OS keychain; delete it there or try again later."
    )
}

/// Nothing was saved or deleted.
pub fn secret_delete_failed_text(kind: McpErrorKind) -> String {
    format!(
        "Nothing was changed: the settings were not saved and the keychain entry was not deleted. {}",
        status_reason_text(kind)
    )
}

/// Why a keychain secret could not be read or stored. evorch never falls
/// back to storing it in a file.
pub fn secret_error_text(kind: McpErrorKind) -> String {
    match kind {
        McpErrorKind::SecretUnavailable => "The OS keychain is not available. Nothing was read or stored, and evorch does not fall back to a file.".into(),
        McpErrorKind::SecretLocked => {
            "The OS keychain is locked. Unlock it and try again.".into()
        }
        McpErrorKind::SecretDenied => {
            "Access to the OS keychain was denied or cancelled. Nothing was stored.".into()
        }
        McpErrorKind::SecretMissing => {
            "The keychain entry has no stored value yet. Edit the server to set it.".into()
        }
        McpErrorKind::ConfigConflict => {
            "The MCP configuration changed on disk. Reload and try again.".into()
        }
        other => format!("The secret was not stored: {}", status_reason_text(other)),
    }
}

/// State, approval and connection-test controls of one server row.
pub(super) fn status_controls(
    ui: &mut Ui,
    model: &McpSettingsModel,
    id: &str,
    entry: &McpServerEntry,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    let runnable = entry
        .definition
        .as_ref()
        .is_some_and(|definition| definition.enabled);
    let note = model.row_note(id);
    let event = model.status(id);
    let detail = match note {
        Some(McpRowNote::Approved | McpRowNote::Revoked) => None,
        _ => event.as_ref().and_then(status_text),
    };
    if let Some(detail) = detail {
        let color = match event.map(|event| event.status) {
            Some(McpServerStatus::Ready | McpServerStatus::Disabled) => palette().TEXT_MUTED,
            Some(McpServerStatus::Invalid | McpServerStatus::Unavailable) => palette().ERROR_FG,
            _ => palette().WARNING_FG,
        };
        ui.colored_label(color, detail);
    }
    if !runnable {
        return;
    }
    let row_busy = model.row_busy(id);
    let needs_approval = model.needs_approval(id);
    ui.horizontal_wrapped(|ui| {
        let review_text = if needs_approval {
            "Approve…"
        } else {
            "Review approval"
        };
        let review = ui.add_enabled(!busy && !row_busy, egui::Button::new(review_text).small());
        accessible(&review, &format!("Review approval {id}"));
        if review.clicked() {
            *action = Some(McpSettingsAction::ReviewApproval(id.to_owned()));
        }
        let test = ui.add_enabled(
            model.can_test(id),
            egui::Button::new("Test connection").small(),
        );
        accessible(&test, &format!("Test connection {id}"));
        if needs_approval {
            test.on_disabled_hover_text("Approve this server before testing it.");
        } else if test.clicked() {
            *action = Some(McpSettingsAction::TestConnection(id.to_owned()));
        }
        if !needs_approval {
            let revoke = ui.add_enabled(
                !busy && !row_busy,
                egui::Button::new("Revoke approval").small(),
            );
            accessible(&revoke, &format!("Revoke approval {id}"));
            if revoke.clicked() {
                *action = Some(McpSettingsAction::RevokeApproval(id.to_owned()));
            }
        }
        if row_busy {
            spinner(ui);
        }
    });
    match note {
        None | Some(McpRowNote::Testing) => {}
        Some(McpRowNote::Approved) => {
            ui.colored_label(palette().SUCCESS, NEXT_RUN_NOTICE);
        }
        Some(McpRowNote::Revoked) => {
            ui.colored_label(palette().WARNING_FG, REVOKED_TEXT);
        }
        Some(McpRowNote::Tested(status)) => {
            let (label, color) = status_badge(*status);
            ui.colored_label(color, format!("Connection test: {label}."));
        }
        Some(McpRowNote::TestRefused) => {
            ui.colored_label(palette().WARNING_FG, TEST_REFUSED_TEXT);
        }
        Some(McpRowNote::Failed(text)) => {
            ui.colored_label(palette().ERROR_FG, text);
        }
    }
}

/// The approval dialog, stacked over the MCP servers dialog.
pub(super) fn approval_modal(
    ctx: &egui::Context,
    model: &McpSettingsModel,
) -> Option<McpSettingsAction> {
    let review = model.approval()?;
    let entry = model
        .config()
        .and_then(|config| config.servers.get(&review.server_id));
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("mcp-approval-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.7).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 2.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3(format!("Approve MCP server {}", review.server_id)));
            egui::ScrollArea::vertical()
                .max_height(ctx.viewport_rect().height() * 0.65)
                .auto_shrink([false, true])
                .show(ui, |ui| match &review.state {
                    McpApprovalState::Loading => {
                        ui.horizontal(|ui| {
                            spinner(ui);
                            ui.label(muted("Inspecting the launch settings…"));
                        });
                    }
                    McpApprovalState::Ready(preview) | McpApprovalState::Confirming(preview) => {
                        preview_body(ui, preview, entry);
                    }
                    McpApprovalState::Stale => {
                        ui.colored_label(palette().WARNING_FG, STALE_PREVIEW_TEXT);
                    }
                    McpApprovalState::Failed(text) => {
                        ui.colored_label(palette().ERROR_FG, text);
                    }
                });
            ui.add_space(SP_1);
            ui.horizontal(|ui| match &review.state {
                McpApprovalState::Ready(_) | McpApprovalState::Confirming(_) => {
                    let ready = matches!(review.state, McpApprovalState::Ready(_));
                    let approve = ui
                        .add_enabled_ui(ready, |ui| primary_button(ui, APPROVE_LABEL))
                        .inner;
                    accessible(&approve, &format!("{APPROVE_LABEL} {}", review.server_id));
                    if approve.clicked() && ready {
                        action = Some(McpSettingsAction::ConfirmApproval);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(McpSettingsAction::CancelApproval);
                    }
                    if !ready {
                        spinner(ui);
                    }
                }
                McpApprovalState::Stale => {
                    if primary_button(ui, REVIEW_AGAIN_LABEL).clicked() {
                        action = Some(McpSettingsAction::ReviewApproval(review.server_id.clone()));
                    }
                    if ui.button("Close").clicked() {
                        action = Some(McpSettingsAction::CancelApproval);
                    }
                }
                McpApprovalState::Loading | McpApprovalState::Failed(_) => {
                    if ui.button("Close").clicked() {
                        action = Some(McpSettingsAction::CancelApproval);
                    }
                }
            });
        });
    let confirming = matches!(review.state, McpApprovalState::Confirming(_));
    if action.is_none() && modal.should_close() && !confirming {
        action = Some(McpSettingsAction::CancelApproval);
    }
    action
}

fn preview_body(ui: &mut Ui, preview: &McpApprovalPreview, entry: Option<&McpServerEntry>) {
    let definition = entry.and_then(|entry| entry.definition.as_ref());
    let stdio = matches!(
        definition.map(|definition| &definition.transport),
        Some(McpTransportDefinition::Stdio { .. })
    ) || preview.summary.executable.is_some();
    if stdio {
        ui.colored_label(palette().WARNING_FG, semibold(OUTSIDE_SANDBOX_WARNING));
    }
    ui.label(muted(UNVERIFIED_CONTENT_NOTE));
    change_line(ui, preview);
    ui.label(h4(if stdio {
        "What runs"
    } else {
        "Where it connects"
    }));
    match definition.map(|definition| &definition.transport) {
        Some(McpTransportDefinition::Stdio {
            command,
            args,
            cwd: _,
            env,
        }) => {
            field(ui, "Command", &template_text(command));
            if let Some(executable) = &preview.summary.executable {
                field(ui, "Resolved program", &executable.display().to_string());
            }
            if args.is_empty() {
                field(ui, "Arguments", "none");
            } else {
                ui.label(muted("Arguments"));
                for (index, arg) in args.iter().enumerate() {
                    code_line(ui, &format!("{}. {}", index + 1, template_text(arg)));
                }
            }
            if let Some(cwd) = &preview.summary.canonical_cwd {
                field(ui, "Working directory", &cwd.display().to_string());
            }
            values(ui, "Environment variables", env);
            ui.label(muted(
                "Besides these, only basic variables such as HOME, PATH, USER, LANG and XDG_* are passed from evorch's environment.",
            ));
        }
        Some(McpTransportDefinition::Http {
            url,
            headers,
            token,
        }) => {
            field(ui, "URL", &template_text(url));
            if let Some(destination) = &preview.summary.destination {
                field(
                    ui,
                    "Destination",
                    &format!(
                        "{}://{}:{}",
                        destination.scheme, destination.host, destination.port
                    ),
                );
            }
            if let Some(token) = token {
                // A bearer token is a credential whatever it is called.
                field(ui, "Bearer token", &value_text(token, true));
            }
            values(ui, "Headers", headers);
        }
        None => summary_only(ui, preview),
    }
    if !preview.summary.secret_references.is_empty() {
        field(
            ui,
            "Secrets used",
            &preview.summary.secret_references.join(", "),
        );
    }
    if !preview.approved_ips.is_empty() {
        let ips: Vec<String> = preview
            .approved_ips
            .iter()
            .map(ToString::to_string)
            .collect();
        field(ui, "Allowed addresses", &ips.join(", "));
    }
    for warning in &preview.warnings {
        // The sandbox warning already leads the dialog for local programs.
        if stdio && matches!(warning, ApprovalWarning::HostProcessOutsideSandbox) {
            continue;
        }
        let (text, color) = warning_text(*warning, preview);
        ui.colored_label(color, text);
    }
    ui.label(muted(
        "Approval applies from the next new run. Conversations already running keep their current tools.",
    ));
}

fn change_line(ui: &mut Ui, preview: &McpApprovalPreview) {
    match (preview.previous_fingerprint, &preview.previous_summary) {
        (None, _) => {
            ui.label(muted("This server has not been approved before."));
        }
        (Some(previous), _) if previous == preview.fingerprint => {
            ui.label(muted(
                "Already approved with exactly these settings. Approving again changes nothing.",
            ));
        }
        (Some(_), None) => {
            ui.colored_label(
                palette().WARNING_FG,
                semibold("Changed since your last approval: the command, arguments, working directory, environment, destination or secret references differ. Check everything below."),
            );
        }
        (Some(_), Some(previous)) => {
            ui.colored_label(
                palette().WARNING_FG,
                semibold("Changed since your last approval:"),
            );
            let changes = summary_changes(previous, &preview.summary);
            for change in &changes {
                ui.colored_label(palette().WARNING_FG, change);
            }
            // Only the summary above is recorded at approval; everything else
            // (command, arguments, URL, plain values, allowed addresses) is
            // compared by fingerprint alone.
            let other = if changes.is_empty() {
                OTHER_CHANGES_TEXT
            } else {
                OTHER_MAYBE_CHANGED_TEXT
            };
            ui.colored_label(palette().WARNING_FG, other);
        }
    }
}

/// One line per item of the recorded approval summary that differs.
fn summary_changes(previous: &McpApprovalSummary, current: &McpApprovalSummary) -> Vec<String> {
    let mut changes = Vec::new();
    let path =
        |path: &Option<std::path::PathBuf>| path.as_ref().map(|path| path.display().to_string());
    let destination = |destination: &Option<McpDestinationSummary>| {
        destination.as_ref().map(|destination| {
            format!(
                "{}://{}:{}",
                destination.scheme, destination.host, destination.port
            )
        })
    };
    for (item, before, after) in [
        (
            "program",
            path(&previous.executable),
            path(&current.executable),
        ),
        (
            "working directory",
            path(&previous.canonical_cwd),
            path(&current.canonical_cwd),
        ),
        (
            "destination",
            destination(&previous.destination),
            destination(&current.destination),
        ),
    ] {
        match (before, after) {
            (Some(before), Some(after)) if before != after => {
                changes.push(format!("Changed: {item} {before} \u{2192} {after}"));
            }
            (None, Some(after)) => changes.push(format!("Added: {item} {after}")),
            (Some(before), None) => changes.push(format!("Removed: {item} {before}")),
            _ => {}
        }
    }
    for (item, before, after) in [
        (
            "environment variable",
            &previous.environment_keys,
            &current.environment_keys,
        ),
        (
            "secret reference",
            &previous.secret_references,
            &current.secret_references,
        ),
    ] {
        let before: BTreeSet<&String> = before.iter().collect();
        let after: BTreeSet<&String> = after.iter().collect();
        for name in after.difference(&before) {
            changes.push(format!("Added: {item} {name}"));
        }
        for name in before.difference(&after) {
            changes.push(format!("Removed: {item} {name}"));
        }
    }
    changes
}

fn warning_text(warning: ApprovalWarning, preview: &McpApprovalPreview) -> (String, egui::Color32) {
    match warning {
        ApprovalWarning::HostProcessOutsideSandbox => {
            (OUTSIDE_SANDBOX_WARNING.into(), palette().WARNING_FG)
        }
        ApprovalWarning::PrivateDestination => (
            "This is an address on this computer or the local network. evorch connects only to the addresses listed above; a new address needs approval again.".into(),
            palette().WARNING_FG,
        ),
        ApprovalWarning::PlaintextHttp => (
            "http:// is not encrypted: requests, headers and any secrets are sent in plain text on the network.".into(),
            palette().ERROR_FG,
        ),
        ApprovalWarning::SendsSecretReferences => (
            format!(
                "These secrets are sent to this server with every request: {}.",
                preview.summary.secret_references.join(", ")
            ),
            palette().WARNING_FG,
        ),
    }
}

/// Without the loaded definition, only the service's safe summary is shown.
fn summary_only(ui: &mut Ui, preview: &McpApprovalPreview) {
    if let Some(executable) = &preview.summary.executable {
        field(ui, "Program", &executable.display().to_string());
    }
    if let Some(cwd) = &preview.summary.canonical_cwd {
        field(ui, "Working directory", &cwd.display().to_string());
    }
    if let Some(destination) = &preview.summary.destination {
        field(
            ui,
            "Destination",
            &format!(
                "{}://{}:{}",
                destination.scheme, destination.host, destination.port
            ),
        );
    }
    if !preview.summary.environment_keys.is_empty() {
        field(
            ui,
            "Environment variables",
            &preview.summary.environment_keys.join(", "),
        );
    }
}

fn values(ui: &mut Ui, title: &str, values: &BTreeMap<String, ValueSource>) {
    if values.is_empty() {
        field(ui, title, "none");
        return;
    }
    ui.label(muted(title));
    for (name, value) in values {
        code_line(
            ui,
            &format!("{name} = {}", value_text(value, looks_secret(name))),
        );
    }
}

/// How a configured value appears in the approval preview. `${VAR}`
/// references and keychain entries are shown by name and never resolved.
/// Plain text is shown so the launch can be judged, except in a value that
/// carries a credential, where only its length is shown.
fn value_text(value: &ValueSource, secret: bool) -> String {
    match value {
        ValueSource::Keychain { name } => format!("keychain entry \"{name}\""),
        ValueSource::Template { template } if !secret => template_text(template),
        ValueSource::Template { template } => template
            .0
            .iter()
            .map(|part| match part {
                TemplatePart::Literal { value } if value.is_empty() => String::new(),
                TemplatePart::Literal { value } => match value.chars().count() {
                    1 => "[hidden text, 1 character]".to_owned(),
                    count => format!("[hidden text, {count} characters]"),
                },
                TemplatePart::Env {
                    name,
                    default: None,
                } => format!("${{{name}}}"),
                TemplatePart::Env {
                    name,
                    default: Some(default),
                } => format!("${{{name}:-{default}}}"),
            })
            .collect(),
    }
}

fn field(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(muted(format!("{label}:")));
        ui.label(RichText::new(value).monospace().color(palette().TEXT));
    });
}

fn code_line(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).monospace().color(palette().TEXT));
}

/// Keychain references of the open draft, with masked input for their values.
pub(super) fn secret_section(
    ui: &mut Ui,
    parts: EditorParts<'_>,
    check: &DraftCheck,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    let EditorParts {
        draft,
        secret,
        stored,
        saved,
        status,
    } = parts;
    ui.label(semibold("OS keychain secrets").color(palette().TEXT));
    let references = draft.keychain_references();
    if references.is_empty() {
        ui.label(muted(
            "To keep a token out of .evorch/mcp.toml, choose Keychain for the bearer token, a header or an environment variable, give the entry a name, then set its value here.",
        ));
        return;
    }
    if let Some(reason) = status.as_ref().and_then(|event| event.reason)
        && matches!(
            reason,
            McpErrorKind::SecretUnavailable
                | McpErrorKind::SecretLocked
                | McpErrorKind::SecretDenied
                | McpErrorKind::SecretMissing
        )
    {
        ui.colored_label(palette().WARNING_FG, secret_error_text(reason));
    }
    let mut remove = None;
    let existing = draft.existing_id.is_some();
    for (target, name) in &references {
        ui.push_id(("secret", target.label()), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{}: keychain entry \"{name}\"", target.label()));
                let (state, color) = if name.is_empty() {
                    ("enter an entry name", palette().WARNING_FG)
                } else if stored.contains(name) {
                    ("stored", palette().SUCCESS)
                } else if saved.contains(name) {
                    ("saved reference, value hidden", palette().TEXT_MUTED)
                } else {
                    ("no value set here yet", palette().WARNING_FG)
                };
                badge(ui, state, color, palette().SURFACE);
                let editing = secret.as_ref().is_some_and(|input| &input.target == target);
                if !editing && !name.is_empty() {
                    let text = if stored.contains(name) || saved.contains(name) {
                        "Replace value"
                    } else {
                        "Set value"
                    };
                    let set = ui.add_enabled(!busy, egui::Button::new(text).small());
                    accessible(&set, &format!("{text} {name}"));
                    if set.clicked() {
                        *action = Some(McpSettingsAction::EditSecret(target.clone()));
                    }
                }
                // An entry the saved server uses is deleted from the keychain
                // too; that also saves the form. Otherwise only the form changes.
                let deletes = existing && (stored.contains(name) || saved.contains(name));
                let (text, hover) = if deletes {
                    (
                        "Delete",
                        "Removes every reference to this entry, saves the server, then deletes the value from the OS keychain.",
                    )
                } else {
                    ("Remove", "Removes the reference from this form.")
                };
                let ready = !busy && (!deletes || check.is_valid());
                let drop = ui.add_enabled(ready, egui::Button::new(text).small());
                accessible(&drop, &format!("{text} secret {}", target.label()));
                let drop = if deletes && !check.is_valid() {
                    drop.on_disabled_hover_text(
                        "Fix the errors above first: deleting also saves this server.",
                    )
                } else {
                    drop.on_hover_text(hover)
                };
                if drop.clicked() {
                    remove = Some(target.clone());
                }
            });
            if let Some(input) = secret.as_mut().filter(|input| &input.target == target) {
                secret_input(ui, input, name, check, busy, action);
            }
        });
    }
    if let Some(target) = remove {
        *action = Some(McpSettingsAction::DeleteSecret(target));
    }
}

fn secret_input(
    ui: &mut Ui,
    input: &mut SecretInput,
    name: &str,
    check: &DraftCheck,
    busy: bool,
    action: &mut Option<McpSettingsAction>,
) {
    let label = format!("Secret value for {name}");
    let response = ui.add(
        egui::TextEdit::singleline(&mut input.value)
            .password(true)
            .hint_text("paste the value")
            .desired_width(ui.available_width()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, &label));
    ui.label(muted(SECRET_STORAGE_NOTE));
    if !check.is_valid() {
        ui.colored_label(
            palette().WARNING_FG,
            "Fix the errors above first: storing the value also saves this server.",
        );
    }
    ui.horizontal(|ui| {
        let ready = !busy && !input.saving && !input.value.is_empty() && check.is_valid();
        let store = ui
            .add_enabled_ui(ready, |ui| primary_button(ui, STORE_SECRET_LABEL))
            .inner;
        accessible(&store, STORE_SECRET_LABEL);
        if store.clicked() && ready {
            *action = Some(McpSettingsAction::SaveSecret);
        }
        if ui.button("Cancel").clicked() {
            *action = Some(McpSettingsAction::CancelSecret);
        }
        if input.saving {
            spinner(ui);
        }
    });
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn preparation_and_role_reasons_do_not_claim_size_or_approval_failures() {
        let preparation = status_reason_text(McpErrorKind::PreparationLimitReached);
        assert!(preparation.contains("prepared") && preparation.contains("this run"));
        assert!(!preparation.contains("size"));
        let role = status_reason_text(McpErrorKind::RoleNotAllowed);
        assert!(role.contains("role"));
        assert!(!role.contains("revoked") && !role.contains("approval"));
    }
}
