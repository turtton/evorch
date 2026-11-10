//! Secret-free consent projections and a fail-closed, user-local approval ledger.
use super::McpErrorKind;
use super::types::{
    ApprovalFingerprint, ApprovalPreviewId, ApprovalWarning, McpApprovalInspection,
    McpApprovalSummary, McpDestinationSummary, canonical_json_bytes,
};
pub use super::types::{
    ApprovalPermit, ApprovalScope, ConfirmMcpApproval, McpApprovalPreview, McpApprovalStore,
};
use config::mcp::{
    McpServerDefinition, McpTransportDefinition, SourceRevision, Template, TemplatePart,
    ValueSource,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::IpAddr,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::NamedTempFile;
use url::Url;

/// Inputs are already inspected canonical paths. This function performs no I/O,
/// environment lookup, secret resolution, process launch or network operation.
pub fn inspect_stdio(
    scope: ApprovalScope,
    revision: SourceRevision,
    definition: &McpServerDefinition,
    executable: &Path,
    canonical_cwd: &Path,
) -> Result<McpApprovalInspection, McpErrorKind> {
    let McpTransportDefinition::Stdio {
        command,
        args,
        env,
        cwd,
    } = &definition.transport
    else {
        return Err(McpErrorKind::Configuration);
    };
    if !executable.is_absolute()
        || !canonical_cwd.is_absolute()
        || !scope.canonical_root.is_absolute()
        || scope.canonical_root.to_str().is_none()
    {
        return Err(McpErrorKind::Configuration);
    }
    let executable_text = executable.to_str().ok_or(McpErrorKind::Configuration)?;
    let cwd_text = canonical_cwd.to_str().ok_or(McpErrorKind::Configuration)?;
    let projection = json!({"transport":"stdio", "command":command, "executable":executable_text,
        "args":args, "cwd":cwd_text, "cwd_references":cwd.as_ref().map(template_reference_projection).unwrap_or_else(||json!([])), "env":env});
    let mut references = Vec::new();
    template_references(command, &mut references);
    if let Some(cwd) = cwd {
        template_references(cwd, &mut references);
    }
    for arg in args {
        template_references(arg, &mut references);
    }
    for source in env.values() {
        source_references(source, &mut references);
    }
    references.sort();
    references.dedup();
    Ok(McpApprovalInspection {
        scope,
        source_revision: revision,
        fingerprint: ApprovalFingerprint::from_projection(&canonical_json_bytes(&projection)),
        summary: McpApprovalSummary {
            executable: Some(executable.into()),
            canonical_cwd: Some(canonical_cwd.into()),
            destination: None,
            environment_keys: env.keys().cloned().collect(),
            secret_references: references,
        },
        warnings: vec![ApprovalWarning::HostProcessOutsideSandbox],
        approved_ips: vec![],
    })
}

/// C supplies the checked, normalized URL. Only its origin is read when URL
/// templates have references; path/query projection uses symbolic references,
/// never the expanded credential. Nonempty IPs mean a destination-bound local
/// exception; public DNS addresses must NOT be included in approved_ips.
pub fn inspect_http(
    scope: ApprovalScope,
    revision: SourceRevision,
    definition: &McpServerDefinition,
    checked_url: &Url,
    mut approved_ips: Vec<IpAddr>,
) -> Result<McpApprovalInspection, McpErrorKind> {
    let McpTransportDefinition::Http {
        url,
        headers,
        token,
    } = &definition.transport
    else {
        return Err(McpErrorKind::Configuration);
    };
    if !scope.canonical_root.is_absolute()
        || scope.canonical_root.to_str().is_none()
        || !matches!(checked_url.scheme(), "http" | "https")
        || checked_url.host_str().is_none()
        || !checked_url.username().is_empty()
        || checked_url.password().is_some()
        || checked_url.fragment().is_some()
    {
        return Err(McpErrorKind::Configuration);
    }
    approved_ips.sort();
    approved_ips.dedup();
    let mut references = Vec::new();
    template_references(url, &mut references);
    let header_refs: BTreeMap<_, _> = headers
        .iter()
        .map(|(name, source)| {
            source_references(source, &mut references);
            (name.to_ascii_lowercase(), http_source_projection(source))
        })
        .collect();
    let token_ref = token.as_ref().map(|source| {
        source_references(source, &mut references);
        http_source_projection(source)
    });
    references.sort();
    references.dedup();
    let destination = McpDestinationSummary {
        scheme: checked_url.scheme().into(),
        host: checked_url.host_str().unwrap_or_default().into(),
        port: checked_url
            .port_or_known_default()
            .ok_or(McpErrorKind::Configuration)?,
    };
    let projection = json!({"transport":"http", "destination":destination,
        "url":url_projection(url)?, "headers":header_refs, "token":token_ref, "ips":approved_ips});
    let mut warnings = Vec::new();
    if !approved_ips.is_empty() {
        warnings.push(ApprovalWarning::PrivateDestination);
    }
    if checked_url.scheme() == "http" {
        warnings.push(ApprovalWarning::PlaintextHttp);
    }
    if !references.is_empty() {
        warnings.push(ApprovalWarning::SendsSecretReferences);
    }
    Ok(McpApprovalInspection {
        scope,
        source_revision: revision,
        fingerprint: ApprovalFingerprint::from_projection(&canonical_json_bytes(&projection)),
        summary: McpApprovalSummary {
            executable: None,
            canonical_cwd: None,
            destination: Some(destination),
            environment_keys: vec![],
            secret_references: references,
        },
        warnings,
        approved_ips,
    })
}
fn url_projection(template: &Template) -> Result<Value, McpErrorKind> {
    let mut symbolic = String::new();
    let mut refs = Vec::new();
    for part in &template.0 {
        match part {
            TemplatePart::Literal { value } => symbolic.push_str(value),
            TemplatePart::Env { name, default } => {
                symbolic.push_str(&format!("evorchref{}", refs.len()));
                refs.push(json!({"name":name,"default":default}));
            }
        }
    }
    if refs.is_empty() {
        let url = Url::parse(&symbolic).map_err(|_| McpErrorKind::Configuration)?;
        return Ok(json!({"literal":url.as_str()}));
    }
    // A whole URL or port may be an environment reference and not parse as a
    // symbolic URL. Retain its AST instead; checked origin still binds routing.
    Ok(match Url::parse(&symbolic) {
        Ok(url) => json!({"symbolic":url.as_str(), "references":refs}),
        Err(_) => json!({"template":template}),
    })
}
fn template_reference_projection(template: &Template) -> Value {
    json!(
        template
            .0
            .iter()
            .filter_map(|part| match part {
                TemplatePart::Env { name, default } => Some(json!({"env":name,"default":default})),
                _ => None,
            })
            .collect::<Vec<_>>()
    )
}
fn http_source_projection(source: &ValueSource) -> Value {
    match source {
        ValueSource::Keychain { name } => json!({"keychain":name}),
        ValueSource::Template { template } => {
            json!({"template":template.0.iter().map(|part| match part {
            TemplatePart::Literal { .. } => json!({"literal":true}),
            TemplatePart::Env { name, default } => json!({"env":name,"default":default}),
        }).collect::<Vec<_>>()})
        }
    }
}
fn template_references(template: &Template, out: &mut Vec<String>) {
    for part in &template.0 {
        if let TemplatePart::Env { name, .. } = part {
            out.push(format!("env:{name}"));
        }
    }
}
fn source_references(source: &ValueSource, out: &mut Vec<String>) {
    match source {
        ValueSource::Keychain { name } => out.push(format!("keychain:{name}")),
        ValueSource::Template { template } => template_references(template, out),
    }
}

const LEDGER_FILE: &str = "mcp-approvals.json";
const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    entries: Vec<Record>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    scope: ApprovalScope,
    fingerprint: ApprovalFingerprint,
    // Bounded by MAX_LEDGER_BYTES; contains no argument/query/header/env values.
    summary: McpApprovalSummary,
    approved_ips: Vec<IpAddr>,
    revoked: bool,
}
#[derive(Default)]
struct State {
    pending: Vec<McpApprovalPreview>,
    live: Vec<(ApprovalScope, ApprovalFingerprint, Weak<AtomicBool>)>,
    denied: Vec<ApprovalScope>,
}
type SharedState = Arc<Mutex<State>>;
static STATES: OnceLock<Mutex<HashMap<PathBuf, SharedState>>> = OnceLock::new();

/// Open is inert. Only explicit ledger operations create directories/files.
/// Clone/share this store across GUI and runs; handles for the same directory
/// also share revocation state within the process.
#[derive(Clone)]
pub struct ApprovalLedger {
    dir: PathBuf,
    state: SharedState,
}
impl ApprovalLedger {
    pub fn open_user() -> Result<Self, McpErrorKind> {
        Self::at_user_config_dir(config::user_config_dir().ok_or(McpErrorKind::Unavailable)?)
    }
    /// Explicit core user configuration directory, also usable by isolated tests.
    pub fn at_user_config_dir(dir: PathBuf) -> Result<Self, McpErrorKind> {
        if !dir.is_absolute() {
            return Err(McpErrorKind::Configuration);
        }
        let mut states = STATES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| McpErrorKind::Unavailable)?;
        // Keep registrations while permits outlive every store handle. A newly
        // opened handle must still be able to revoke those live permits.
        states.retain(|_, state| {
            Arc::strong_count(state) > 1
                || state.lock().is_ok_and(|s| {
                    !s.denied.is_empty()
                        || s.live.iter().any(|(_, _, flag)| flag.strong_count() > 0)
                })
        });
        let state = states.entry(dir.clone()).or_default().clone();
        Ok(Self { dir, state })
    }
    fn locked_file(&self) -> Result<File, McpErrorKind> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&self.dir)
            .map_err(|_| McpErrorKind::Unavailable)?;
        let meta = fs::symlink_metadata(&self.dir).map_err(|_| McpErrorKind::Unavailable)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(McpErrorKind::Unavailable);
        }
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| McpErrorKind::Unavailable)?;
        let file = secure_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(".mcp-approvals.lock"))
            .map_err(|_| McpErrorKind::Unavailable)?;
        if !file
            .metadata()
            .map_err(|_| McpErrorKind::Unavailable)?
            .is_file()
        {
            return Err(McpErrorKind::Unavailable);
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| McpErrorKind::Unavailable)?;
        file.lock().map_err(|_| McpErrorKind::Unavailable)?;
        Ok(file)
    }
    fn load(&self) -> Option<Ledger> {
        let file = match secure_options().read(true).open(self.dir.join(LEDGER_FILE)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(Ledger::default());
            }
            Err(_) => return None,
        };
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_LEDGER_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }
    fn save(&self, ledger: &Ledger) -> Result<(), McpErrorKind> {
        let mut temp = NamedTempFile::new_in(&self.dir).map_err(|_| McpErrorKind::Unavailable)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| McpErrorKind::Unavailable)?;
        let bytes = serde_json::to_vec(ledger).map_err(|_| McpErrorKind::Unavailable)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        temp.write_all(&bytes)
            .map_err(|_| McpErrorKind::Unavailable)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| McpErrorKind::Unavailable)?;
        temp.persist(self.dir.join(LEDGER_FILE))
            .map_err(|_| McpErrorKind::Unavailable)?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| McpErrorKind::Unavailable)
    }
}
fn secure_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    options
}
fn invalidate(state: &mut State, scope: &ApprovalScope) {
    state.pending.retain(|preview| &preview.scope != scope);
    invalidate_live(state, scope);
}
fn invalidate_live(state: &mut State, scope: &ApprovalScope) {
    invalidate_live_except(state, scope, None);
}
fn invalidate_live_except(
    state: &mut State,
    scope: &ApprovalScope,
    keep: Option<ApprovalFingerprint>,
) {
    state.live.retain(|(old_scope, fingerprint, flag)| {
        if old_scope == scope && Some(*fingerprint) != keep {
            if let Some(flag) = flag.upgrade() {
                flag.store(true, Ordering::Release);
            }
            false
        } else {
            flag.strong_count() > 0
        }
    });
}
fn same_identity(preview: &McpApprovalPreview, current: &McpApprovalInspection) -> bool {
    preview.scope == current.scope
        && preview.source_revision == current.source_revision
        && preview.fingerprint == current.fingerprint
        && preview.approved_ips == current.approved_ips
        && serde_json::to_value(&preview.summary).ok()
            == serde_json::to_value(&current.summary).ok()
        && serde_json::to_value(&preview.warnings).ok()
            == serde_json::to_value(&current.warnings).ok()
}
impl McpApprovalStore for ApprovalLedger {
    fn approved_ips(&self, scope: &ApprovalScope) -> Result<Vec<IpAddr>, McpErrorKind> {
        let state = self.state.lock().map_err(|_| McpErrorKind::Unavailable)?;
        if state.denied.contains(scope) {
            return Err(McpErrorKind::NeedsApproval);
        }
        let _lock = self
            .locked_file()
            .map_err(|_| McpErrorKind::NeedsApproval)?;
        let ledger = self.load().ok_or(McpErrorKind::NeedsApproval)?;
        match ledger
            .entries
            .into_iter()
            .find(|record| &record.scope == scope)
        {
            Some(record) if record.revoked => Err(McpErrorKind::NeedsApproval),
            Some(record) => Ok(record.approved_ips),
            None => Ok(Vec::new()),
        }
    }
    fn preview(&self, current: &McpApprovalInspection) -> Result<McpApprovalPreview, McpErrorKind> {
        let mut state = self.state.lock().map_err(|_| McpErrorKind::Unavailable)?;
        let _lock = self.locked_file()?;
        let previous = self.load().and_then(|ledger| {
            ledger
                .entries
                .into_iter()
                .find(|r| r.scope == current.scope)
        });
        let preview = McpApprovalPreview {
            id: ApprovalPreviewId(*uuid::Uuid::new_v4().as_bytes()),
            scope: current.scope.clone(),
            source_revision: current.source_revision,
            fingerprint: current.fingerprint,
            warnings: current.warnings.clone(),
            previous_fingerprint: previous
                .as_ref()
                .filter(|r| !r.revoked)
                .map(|r| r.fingerprint),
            previous_summary: previous.filter(|r| !r.revoked).map(|r| r.summary),
            summary: current.summary.clone(),
            approved_ips: current.approved_ips.clone(),
        };
        state.pending.retain(|p| p.scope != current.scope);
        if state.pending.len() >= 256 {
            state.pending.remove(0);
        }
        state.pending.push(preview.clone());
        Ok(preview)
    }
    fn approve(
        &self,
        confirmation: &ConfirmMcpApproval,
        current: &McpApprovalInspection,
    ) -> Result<(), McpErrorKind> {
        let mut state = self.state.lock().map_err(|_| McpErrorKind::Unavailable)?;
        let index = state
            .pending
            .iter()
            .position(|p| p.id == confirmation.preview_id)
            .ok_or(McpErrorKind::StalePreview)?;
        // Consume even failed confirmations. Re-preview is always possible.
        let preview = state.pending.remove(index);
        if confirmation.scope != preview.scope
            || confirmation.expected_revision != preview.source_revision
            || confirmation.expected_fingerprint != preview.fingerprint
            || !same_identity(&preview, current)
        {
            return Err(McpErrorKind::StalePreview);
        }
        let _lock = self.locked_file()?;
        let mut ledger = self.load().unwrap_or_default();
        let same_grant = !state.denied.contains(&current.scope)
            && ledger.entries.iter().any(|record| {
                record.scope == current.scope
                    && !record.revoked
                    && record.fingerprint == current.fingerprint
                    && record.approved_ips == current.approved_ips
            });
        ledger
            .entries
            .retain(|record| record.scope != current.scope);
        ledger.entries.push(Record {
            scope: current.scope.clone(),
            fingerprint: current.fingerprint,
            summary: current.summary.clone(),
            approved_ips: current.approved_ips.clone(),
            revoked: false,
        });
        self.save(&ledger)?;
        state.denied.retain(|scope| scope != &current.scope);
        if same_grant {
            // Keep only the identical grant, without clearing any revoked flag.
            invalidate_live_except(&mut state, &current.scope, Some(current.fingerprint));
        } else {
            invalidate(&mut state, &current.scope);
        }
        Ok(())
    }
    fn revoke(&self, scope: &ApprovalScope) -> Result<(), McpErrorKind> {
        let mut state = self.state.lock().map_err(|_| McpErrorKind::Unavailable)?;
        // Failed persistence must not resurrect this process's revoked sessions.
        if !state.denied.contains(scope) {
            state.denied.push(scope.clone());
        }
        invalidate(&mut state, scope);
        let _lock = self.locked_file()?;
        let mut ledger = self.load().unwrap_or_default();
        ledger.entries.retain(|record| &record.scope != scope);
        ledger.entries.push(Record {
            scope: scope.clone(),
            fingerprint: ApprovalFingerprint([0; 32]),
            summary: McpApprovalSummary::default(),
            approved_ips: vec![],
            revoked: true,
        });
        self.save(&ledger)?;
        // The durable tombstone now guards new checks; retain process-local
        // denials only if persistence failed above.
        state.denied.retain(|denied| denied != scope);
        Ok(())
    }
    fn check(&self, current: &McpApprovalInspection) -> Result<ApprovalPermit, McpErrorKind> {
        let mut state = self.state.lock().map_err(|_| McpErrorKind::Unavailable)?;
        if state.denied.contains(&current.scope) {
            return Err(McpErrorKind::NeedsApproval);
        }
        let _lock = self.locked_file().map_err(|_| {
            invalidate_live(&mut state, &current.scope);
            McpErrorKind::NeedsApproval
        })?;
        let approved = self.load().is_some_and(|ledger| {
            match ledger
                .entries
                .iter()
                .find(|record| record.scope == current.scope)
            {
                Some(record) => {
                    !record.revoked
                        && (!requires_confirmation(current)
                            || (record.fingerprint == current.fingerprint
                                && record.approved_ips == current.approved_ips))
                }
                None => !requires_confirmation(current),
            }
        });
        if !approved {
            invalidate_live(&mut state, &current.scope);
            return Err(McpErrorKind::NeedsApproval);
        }
        let revoked = Arc::new(AtomicBool::new(false));
        state.live.retain(|(_, _, flag)| flag.strong_count() > 0);
        state.live.push((
            current.scope.clone(),
            current.fingerprint,
            Arc::downgrade(&revoked),
        ));
        Ok(ApprovalPermit {
            scope: current.scope.clone(),
            fingerprint: current.fingerprint,
            revoked,
        })
    }
}

/// The service's checked public HTTPS destination needs no GUI consent unless
/// a reference will be sent. A literal header is deliberately not a reference.
pub fn requires_confirmation(current: &McpApprovalInspection) -> bool {
    !current.warnings.is_empty()
        || !current.approved_ips.is_empty()
        || !current.summary.secret_references.is_empty()
        || current.summary.executable.is_some()
        || current
            .summary
            .destination
            .as_ref()
            .is_none_or(|d| d.scheme != "https")
}
