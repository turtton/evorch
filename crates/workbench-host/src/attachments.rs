//! One disk-backed attachment ingress for native and IPC clients.
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::fs::{FlockOperation, OFlags, flock};
use sandbox::attachments::{AttachmentRoot, PrivateDirectory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use workbench_protocol::{
    commands::AttachmentRef,
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::{AttachmentId, ProjectId, ThreadId},
};

pub const FILE_BYTES: u64 = workbench_protocol::conversation::ATTACHMENT_FILE_BYTES;
pub const CHUNK_BYTES: usize = 256 * 1024;
pub const THREAD_PENDING_COUNT: usize = 32;
pub const THREAD_PENDING_BYTES: u64 = 64 * 1024 * 1024;
pub const STORE_PENDING_COUNT: usize = 256;
pub const STORE_PENDING_BYTES: u64 = 256 * 1024 * 1024;
pub const THREAD_SENT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAttachment {
    pub draft_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for NativeAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAttachment")
            .field("draft_id", &self.draft_id)
            .field("byte_length", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct AttachmentAuthorizer {
    pub(crate) runtime: runtime::AgentRuntime,
    pub(crate) sidebar: Option<PathBuf>,
}
impl AttachmentAuthorizer {
    pub fn check(&self, thread: &str) -> Result<(), WorkbenchError> {
        let deleted = self
            .runtime
            .with_storage_reader(|db| Ok(db.tombstones()?.contains_thread(thread)))
            .map_err(|_| {
                error(
                    ErrorCode::StorageUnavailable,
                    "cannot check attachment tombstones",
                )
            })?
            .unwrap_or(false);
        if deleted {
            return Err(error(ErrorCode::Conflict, "attachment thread was deleted"));
        }
        if let Some(path) = &self.sidebar {
            let store = workspace_state::SidebarStore::new(path.clone());
            let lock = store
                .lock()
                .map_err(|_| error(ErrorCode::Busy, "attachment sidebar is busy; retry"))?;
            let document = store.read(&lock).map_err(|_| {
                error(
                    ErrorCode::StorageUnavailable,
                    "attachment sidebar unavailable",
                )
            })?;
            if document
                .sidebar
                .resolve_states()
                .get(&workspace_state::ThreadId::new(thread))
                .is_some_and(|state| state.archived)
            {
                return Err(error(
                    ErrorCode::Conflict,
                    "thread is archived; restore it before attaching files",
                ));
            }
        }
        Ok(())
    }
}

/// A scoped native upload handle. Calling it on a worker keeps file I/O off the
/// render thread; native bytes and IPC chunks still use AttachmentService.
pub struct AttachmentWriter {
    pub(crate) root: AttachmentRoot,
    pub(crate) project: ProjectId,
    pub(crate) thread: ThreadId,
    pub(crate) authorizer: AttachmentAuthorizer,
}
impl AttachmentWriter {
    pub fn upload(self, draft: NativeAttachment) -> Result<AttachmentRef, WorkbenchError> {
        let mut service = AttachmentService::new(self.root);
        let result = service.upload_bytes_checked(
            self.project,
            self.thread,
            draft.draft_id,
            draft.filename,
            draft.media_type,
            &draft.bytes,
            |thread| self.authorizer.check(thread),
        );
        // Uncommitted bytes are never promoted on a failed upload.
        if result.is_err() {
            let ids = service.uploads.keys().cloned().collect::<Vec<_>>();
            for id in ids {
                if let Ok(id) = AttachmentId::new(id) {
                    let _ = service.discard(&id);
                }
            }
        }
        result
    }

    pub fn discard(self, id: &AttachmentId) -> Result<(), WorkbenchError> {
        let _lock = self.root.lock_thread(self.thread.as_str()).map_err(io)?;
        let parent = self.root.prepare().map_err(io)?;
        let directory =
            match parent.existing_directory(&AttachmentRoot::thread_name(self.thread.as_str())) {
                Ok(directory) => directory,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(io(e)),
            };
        let meta = directory.existing_directory("meta").map_err(io)?;
        let m = match metadata(&meta, &format!("{id}.json")) {
            Ok(m) => m,
            Err(e) => return Err(e),
        };
        if m.project != self.project || m.thread != self.thread {
            return Err(error(
                ErrorCode::PermissionDenied,
                "attachment belongs to another thread",
            ));
        }
        let files = directory.existing_directory("files").map_err(io)?;
        if files.names().map_err(io)?.contains(&m.disk_name) {
            return Ok(());
        } // committed attachments are immutable
        let staging = directory.existing_directory("staging").map_err(io)?;
        staging.remove_file(&format!("{id}.part")).map_err(io)?;
        staging.remove_file(&format!("{id}.ready")).map_err(io)?;
        meta.remove_file(&format!("{id}.json")).map_err(io)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAttachment {
    Uploaded(AttachmentRef),
    Native(NativeAttachment),
}

impl ChatAttachment {
    pub fn immutable_metadata(&self) -> (String, String, u64, String) {
        match self {
            Self::Uploaded(r) => (
                r.filename.clone(),
                r.media_type.clone(),
                r.byte_length,
                r.sha256.clone(),
            ),
            Self::Native(d) => (
                d.filename.clone(),
                if d.media_type.is_empty() {
                    "application/octet-stream".into()
                } else {
                    d.media_type.clone()
                },
                d.bytes.len() as u64,
                format!("{:x}", Sha256::digest(&d.bytes)),
            ),
        }
    }
}

fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

fn io(error: std::io::Error) -> WorkbenchError {
    // Never include data or a client-supplied name in logs/errors.
    WorkbenchError::new(
        ErrorCode::StorageUnavailable,
        format!("attachment store I/O: {}", error.kind()),
        RetryAdvice::Never,
    )
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    project: ProjectId,
    thread: ThreadId,
    client_upload_id: String,
    creator_pid: u32,
    reference: AttachmentRef,
    disk_name: String,
}

struct Upload {
    metadata: Metadata,
    file: File,
    received: u64,
    digest: Sha256,
    finished: bool,
}

pub struct AttachmentService {
    root: AttachmentRoot,
    uploads: BTreeMap<String, Upload>,
}

fn validate_filename(filename: &str) -> Result<(), WorkbenchError> {
    if filename.is_empty()
        || filename.len() > 255
        || matches!(filename, "." | "..")
        || filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(error(
            ErrorCode::InvalidRequest,
            "invalid attachment display filename",
        ));
    }
    Ok(())
}

fn extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 12 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
        .to_ascii_lowercase()
}

fn metadata(directory: &PrivateDirectory, name: &str) -> Result<Metadata, WorkbenchError> {
    let file = directory.file(name, OFlags::RDONLY).map_err(io)?;
    serde_json::from_reader(file.take(16 * 1024))
        .map_err(|_| error(ErrorCode::InvalidRequest, "invalid attachment metadata"))
}

fn write_metadata(directory: &PrivateDirectory, value: &Metadata) -> Result<(), WorkbenchError> {
    let name = format!("{}.json", value.reference.id);
    let temp = format!("{}.tmp", value.reference.id);
    let mut file = directory
        .file(&temp, OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY)
        .map_err(io)?;
    serde_json::to_writer(&mut file, value).map_err(|_| {
        error(
            ErrorCode::InvalidRequest,
            "cannot encode attachment metadata",
        )
    })?;
    file.sync_all().map_err(io)?;
    directory.rename_to(&temp, directory, &name).map_err(io)
}

impl AttachmentService {
    pub fn new(root: AttachmentRoot) -> Self {
        Self {
            root,
            uploads: BTreeMap::new(),
        }
    }

    pub fn recover_dead_uploads(&self) -> Result<(), WorkbenchError> {
        if !self.root.path().try_exists().map_err(io)? {
            return Ok(());
        }
        let _global = self.root.lock("store").map_err(io)?;
        self.reclaim_dead_locked()
    }

    pub fn root(&self) -> &AttachmentRoot {
        &self.root
    }

    pub fn references(
        &self,
        project: &ProjectId,
        thread: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<AttachmentRef>, WorkbenchError> {
        let meta = self
            .root
            .thread(thread.as_str())
            .map_err(io)?
            .directory("meta")
            .map_err(io)?;
        ids.iter()
            .map(|id| {
                let m = metadata(&meta, &format!("{id}.json"))?;
                if &m.project != project || &m.thread != thread {
                    return Err(error(
                        ErrorCode::PermissionDenied,
                        "attachment belongs to another project or thread",
                    ));
                }
                if m.reference.sha256.is_empty() {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "attachment upload has not finished",
                    ));
                }
                Ok(m.reference)
            })
            .collect()
    }

    pub fn begin(
        &mut self,
        project: ProjectId,
        thread: ThreadId,
        client_upload_id: String,
        filename: String,
        media_type_hint: String,
        byte_length: u64,
    ) -> Result<AttachmentId, WorkbenchError> {
        self.begin_checked(
            project,
            thread,
            client_upload_id,
            filename,
            media_type_hint,
            byte_length,
            |_| Ok(()),
        )
    }

    /// Recheck authorization after acquiring the cross-process thread lock.
    pub fn begin_checked(
        &mut self,
        project: ProjectId,
        thread: ThreadId,
        client_upload_id: String,
        filename: String,
        media_type_hint: String,
        byte_length: u64,
        check: impl Fn(&str) -> Result<(), WorkbenchError>,
    ) -> Result<AttachmentId, WorkbenchError> {
        validate_filename(&filename)?;
        let media_type_hint = if media_type_hint.is_empty() {
            "application/octet-stream".to_owned()
        } else {
            media_type_hint
        };
        if byte_length > FILE_BYTES
            || client_upload_id.is_empty()
            || client_upload_id.len() > 128
            || client_upload_id.chars().any(char::is_control)
            || media_type_hint.len() > 128
            || media_type_hint.chars().any(char::is_control)
        {
            return Err(error(
                ErrorCode::LimitExceeded,
                "attachment declaration exceeds limits",
            ));
        }
        let _global = self.root.lock("store").map_err(io)?;
        let _thread = self.root.lock_thread(thread.as_str()).map_err(io)?;
        check(thread.as_str())?;
        for upload in self.uploads.values() {
            let m = &upload.metadata;
            if m.project == project && m.thread == thread && m.client_upload_id == client_upload_id
            {
                if m.reference.filename != filename
                    || m.reference.byte_length != byte_length
                    || m.reference.media_type != media_type_hint
                {
                    return Err(error(
                        ErrorCode::Conflict,
                        "client upload id was already used with different metadata",
                    ));
                }
                return Ok(m.reference.id.clone());
            }
        }
        self.reclaim_dead_locked()?;
        let mut total = (0usize, 0u64);
        let mut own = (0usize, 0u64);
        let root = self.root.prepare().map_err(io)?;
        for name in root
            .names()
            .map_err(io)?
            .into_iter()
            .filter(|n| n.starts_with("t."))
        {
            let directory = root.existing_directory(&name).map_err(io)?;
            let staging = directory.existing_directory("staging").map_err(io)?;
            let meta = directory.existing_directory("meta").map_err(io)?;
            for item in staging
                .names()
                .map_err(io)?
                .into_iter()
                .filter(|n| n.ends_with(".part") || n.ends_with(".ready"))
            {
                let id = item.rsplit_once('.').unwrap().0;
                let m = metadata(&meta, &format!("{id}.json"))?;
                total.0 += 1;
                total.1 = total.1.saturating_add(m.reference.byte_length);
                if m.thread == thread {
                    own.0 += 1;
                    own.1 = own.1.saturating_add(m.reference.byte_length);
                }
            }
        }
        if own.0 >= THREAD_PENDING_COUNT
            || own.1.saturating_add(byte_length) > THREAD_PENDING_BYTES
            || total.0 >= STORE_PENDING_COUNT
            || total.1.saturating_add(byte_length) > STORE_PENDING_BYTES
        {
            return Err(error(
                ErrorCode::LimitExceeded,
                "pending attachment capacity reached; discard unused attachments",
            ));
        }
        let directory = self.root.thread(thread.as_str()).map_err(io)?;
        let staging = directory.directory("staging").map_err(io)?;
        let meta = directory.directory("meta").map_err(io)?;
        directory.directory("files").map_err(io)?;
        let id = AttachmentId::new(uuid::Uuid::new_v4().to_string()).expect("uuid id");
        let m = Metadata {
            project,
            thread,
            client_upload_id,
            creator_pid: std::process::id(),
            disk_name: format!("{id}.{}", extension(&filename)),
            reference: AttachmentRef {
                id: id.clone(),
                filename,
                media_type: media_type_hint,
                byte_length,
                sha256: String::new(),
            },
        };
        let file = staging
            .file(
                &format!("{id}.part"),
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
            )
            .map_err(io)?;
        flock(&file, FlockOperation::LockExclusive).map_err(|e| io(e.into()))?;
        if let Err(error) = write_metadata(&meta, &m) {
            let _ = staging.remove_file(&format!("{id}.part"));
            return Err(error);
        }
        self.uploads.insert(
            id.to_string(),
            Upload {
                metadata: m,
                file,
                received: 0,
                digest: Sha256::new(),
                finished: false,
            },
        );
        Ok(id)
    }

    pub fn append_checked(
        &mut self,
        id: &AttachmentId,
        offset: u64,
        bytes: &[u8],
        check: impl Fn(&str) -> Result<(), WorkbenchError>,
    ) -> Result<u64, WorkbenchError> {
        let upload = self
            .uploads
            .get_mut(id.as_str())
            .ok_or_else(|| error(ErrorCode::NotFound, "upload is unknown in this core epoch"))?;
        let _thread = self
            .root
            .lock_thread(upload.metadata.thread.as_str())
            .map_err(io)?;
        check(upload.metadata.thread.as_str())?;
        if upload.file.metadata().map_err(io)?.nlink() != 1 {
            return Err(error(
                ErrorCode::Conflict,
                "upload was discarded; create a new draft",
            ));
        }
        self.root
            .thread(upload.metadata.thread.as_str())
            .map_err(io)?
            .validate_current()
            .map_err(io)?;
        if upload.finished
            || offset != upload.received
            || bytes.len() > CHUNK_BYTES
            || upload.received.saturating_add(bytes.len() as u64)
                > upload.metadata.reference.byte_length
        {
            return Err(error(
                ErrorCode::InvalidRequest,
                "invalid attachment offset or chunk length",
            ));
        }
        upload.file.write_all(bytes).map_err(io)?;
        upload.digest.update(bytes);
        upload.received += bytes.len() as u64;
        Ok(upload.received)
    }

    pub fn append_base64_checked(
        &mut self,
        id: &AttachmentId,
        offset: u64,
        data: &str,
        check: impl Fn(&str) -> Result<(), WorkbenchError>,
    ) -> Result<u64, WorkbenchError> {
        if data.len() > CHUNK_BYTES.div_ceil(3) * 4 {
            return Err(error(
                ErrorCode::LimitExceeded,
                "attachment chunk exceeds limit",
            ));
        }
        let bytes = STANDARD.decode(data).map_err(|_| {
            error(
                ErrorCode::InvalidRequest,
                "attachment data must be canonical base64",
            )
        })?;
        if STANDARD.encode(&bytes) != data {
            return Err(error(
                ErrorCode::InvalidRequest,
                "attachment data must be canonical base64",
            ));
        }
        self.append_checked(id, offset, &bytes, check)
    }

    pub fn finish_checked(
        &mut self,
        id: &AttachmentId,
        check: impl Fn(&str) -> Result<(), WorkbenchError>,
    ) -> Result<AttachmentRef, WorkbenchError> {
        let upload = self
            .uploads
            .get_mut(id.as_str())
            .ok_or_else(|| error(ErrorCode::NotFound, "upload is unknown in this core epoch"))?;
        let _thread = self
            .root
            .lock_thread(upload.metadata.thread.as_str())
            .map_err(io)?;
        check(upload.metadata.thread.as_str())?;
        if upload.file.metadata().map_err(io)?.nlink() != 1 {
            return Err(error(
                ErrorCode::Conflict,
                "upload was discarded; create a new draft",
            ));
        }
        if upload.finished {
            return Ok(upload.metadata.reference.clone());
        }
        if upload.received != upload.metadata.reference.byte_length {
            return Err(error(
                ErrorCode::InvalidRequest,
                "attachment received length differs from declaration",
            ));
        }
        upload.file.sync_all().map_err(io)?;
        upload.file.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut verify = Sha256::new();
        std::io::copy(&mut upload.file, &mut verify).map_err(io)?;
        let expected = upload.digest.clone().finalize();
        if verify.finalize() != expected {
            return Err(error(
                ErrorCode::InvalidRequest,
                "attachment checksum mismatch",
            ));
        }
        upload.metadata.reference.sha256 = format!("{expected:x}");
        let directory = self
            .root
            .thread(upload.metadata.thread.as_str())
            .map_err(io)?;
        write_metadata(&directory.directory("meta").map_err(io)?, &upload.metadata)?;
        let staging = directory.directory("staging").map_err(io)?;
        staging
            .rename_to(&format!("{id}.part"), &staging, &format!("{id}.ready"))
            .map_err(io)?;
        upload.finished = true;
        Ok(upload.metadata.reference.clone())
    }

    pub fn append(
        &mut self,
        id: &AttachmentId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, WorkbenchError> {
        self.append_checked(id, offset, bytes, |_| Ok(()))
    }
    pub fn append_base64(
        &mut self,
        id: &AttachmentId,
        offset: u64,
        data: &str,
    ) -> Result<u64, WorkbenchError> {
        self.append_base64_checked(id, offset, data, |_| Ok(()))
    }
    pub fn finish(&mut self, id: &AttachmentId) -> Result<AttachmentRef, WorkbenchError> {
        self.finish_checked(id, |_| Ok(()))
    }
    pub fn upload_bytes(
        &mut self,
        project: ProjectId,
        thread: ThreadId,
        draft: String,
        filename: String,
        media_type: String,
        bytes: &[u8],
    ) -> Result<AttachmentRef, WorkbenchError> {
        self.upload_bytes_checked(project, thread, draft, filename, media_type, bytes, |_| {
            Ok(())
        })
    }

    /// Native clients use bytes directly; base64 is exclusively an IPC transport.
    pub fn upload_bytes_checked(
        &mut self,
        project: ProjectId,
        thread: ThreadId,
        draft: String,
        filename: String,
        media_type: String,
        bytes: &[u8],
        check: impl Fn(&str) -> Result<(), WorkbenchError>,
    ) -> Result<AttachmentRef, WorkbenchError> {
        let id = self.begin_checked(
            project,
            thread,
            draft,
            filename,
            media_type,
            bytes.len() as u64,
            &check,
        )?;
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            self.append_checked(&id, (index * CHUNK_BYTES) as u64, chunk, &check)?;
        }
        self.finish_checked(&id, check)
    }

    /// The caller holds `root.lock_thread` until ingress has accepted the chat,
    /// and rechecks archive/deletion under that same lock before invoking this.
    pub fn commit_locked(
        &mut self,
        project: &ProjectId,
        thread: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<(AttachmentRef, PathBuf)>, WorkbenchError> {
        if ids.len() > THREAD_PENDING_COUNT
            || ids
                .iter()
                .map(AttachmentId::as_str)
                .collect::<BTreeSet<_>>()
                .len()
                != ids.len()
        {
            return Err(error(
                ErrorCode::InvalidRequest,
                "too many or duplicate attachments",
            ));
        }
        let directory = self.root.thread(thread.as_str()).map_err(io)?;
        let files = directory.directory("files").map_err(io)?;
        let staging = directory.directory("staging").map_err(io)?;
        let meta = directory.directory("meta").map_err(io)?;
        let mut retained = 0u64;
        for name in files.names().map_err(io)? {
            retained = retained.saturating_add(
                files
                    .file(&name, OFlags::RDONLY)
                    .map_err(io)?
                    .metadata()
                    .map_err(io)?
                    .len(),
            );
        }
        let mut result = Vec::new();
        for id in ids {
            let m = metadata(&meta, &format!("{id}.json"))?;
            if &m.project != project || &m.thread != thread {
                return Err(error(
                    ErrorCode::PermissionDenied,
                    "attachment belongs to another project or thread",
                ));
            }
            if m.reference.sha256.is_empty() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "attachment upload has not finished",
                ));
            }
            let already_sent = files.names().map_err(io)?.contains(&m.disk_name);
            if !already_sent {
                retained = retained.saturating_add(m.reference.byte_length);
            }
            if retained > THREAD_SENT_BYTES {
                return Err(error(
                    ErrorCode::LimitExceeded,
                    "attachment retention limit reached; archive this thread or use a new thread",
                ));
            }
            result.push((m.reference.clone(), files.path().join(&m.disk_name)));
        }
        for (reference, path) in &result {
            let name = path.file_name().unwrap().to_str().unwrap();
            if !files.names().map_err(io)?.iter().any(|n| n == name) {
                staging
                    .rename_to(&format!("{}.ready", reference.id), &files, name)
                    .map_err(io)?;
            }
            self.uploads.remove(reference.id.as_str());
        }
        Ok(result)
    }

    pub fn discard(&mut self, id: &AttachmentId) -> Result<(), WorkbenchError> {
        let Some(upload) = self.uploads.get(id.as_str()) else {
            return Ok(());
        };
        let _thread = self
            .root
            .lock_thread(upload.metadata.thread.as_str())
            .map_err(io)?;
        let directory =
            match self
                .root
                .prepare()
                .map_err(io)?
                .existing_directory(&AttachmentRoot::thread_name(
                    upload.metadata.thread.as_str(),
                )) {
                Ok(directory) => directory,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.uploads.remove(id.as_str());
                    return Ok(());
                }
                Err(e) => return Err(io(e)),
            };
        let staging = directory.existing_directory("staging").map_err(io)?;
        staging.remove_file(&format!("{id}.part")).map_err(io)?;
        staging.remove_file(&format!("{id}.ready")).map_err(io)?;
        directory
            .existing_directory("meta")
            .map_err(io)?
            .remove_file(&format!("{id}.json"))
            .map_err(io)?;
        self.uploads.remove(id.as_str());
        Ok(())
    }

    /// Reclaim only after both creator death and an unlocked data fd are proven.
    /// Startup/upload cleanup never removes another process's active upload.
    fn reclaim_dead_locked(&self) -> Result<(), WorkbenchError> {
        let root = self.root.prepare().map_err(io)?;
        for name in root
            .names()
            .map_err(io)?
            .into_iter()
            .filter(|n| n.starts_with("t."))
        {
            let directory = root.existing_directory(&name).map_err(io)?;
            let staging = directory.existing_directory("staging").map_err(io)?;
            let meta = directory.existing_directory("meta").map_err(io)?;
            for name in staging.names().map_err(io)? {
                let Some((id, ext)) = name.rsplit_once('.') else {
                    continue;
                };
                if !matches!(ext, "part" | "ready") || self.uploads.contains_key(id) {
                    continue;
                }
                let creator = metadata(&meta, &format!("{id}.json"))?.creator_pid;
                // Missing permission or a reused PID is conservative: do not reclaim.
                let dead = rustix::process::Pid::from_raw(creator as i32).is_some_and(|pid| {
                    matches!(
                        rustix::process::test_kill_process(pid),
                        Err(rustix::io::Errno::SRCH)
                    )
                });
                if !dead {
                    continue;
                }
                let file = staging.file(&name, OFlags::RDWR).map_err(io)?;
                match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                    Ok(()) => {
                        staging.remove_file(&name).map_err(io)?;
                        meta.remove_file(&format!("{id}.json")).map_err(io)?;
                    }
                    Err(rustix::io::Errno::WOULDBLOCK) => {}
                    Err(e) => return Err(io(e.into())),
                }
            }
        }
        Ok(())
    }
}

pub fn message_with_paths(text: &str, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return text.to_owned();
    }
    let mut message = text.to_owned();
    if !message.is_empty() {
        message.push_str("\n\n");
    }
    message.push_str("[Attached files (read-only; open with read or shell)]");
    for path in paths {
        message.push('\n');
        message.push_str(&serde_json::to_string(path).expect("attachment path is UTF-8"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, AttachmentService) {
        let temp = tempfile::tempdir().unwrap();
        let root = AttachmentRoot::at(temp.path().join("attachments"));
        (temp, AttachmentService::new(root))
    }
    fn project() -> ProjectId {
        ProjectId::new("project").unwrap()
    }
    fn thread() -> ThreadId {
        ThreadId::new("thread").unwrap()
    }
    fn begin(s: &mut AttachmentService, length: u64) -> Result<AttachmentId, WorkbenchError> {
        s.begin(
            project(),
            thread(),
            uuid::Uuid::new_v4().to_string(),
            "document.pdf".into(),
            "application/pdf".into(),
            length,
        )
    }

    #[test]
    fn arbitrary_bytes_commit_once_and_restart_keeps_the_exact_path() {
        let (_temp, mut s) = service();
        let bytes = b"\0not an image\xff";
        let r = s
            .upload_bytes(
                project(),
                thread(),
                "draft".into(),
                "report.PDF".into(),
                "application/pdf".into(),
                bytes,
            )
            .unwrap();
        assert_eq!(r.sha256, format!("{:x}", Sha256::digest(bytes)));
        let root = s.root.clone();
        let _lock = root.lock_thread(thread().as_str()).unwrap();
        let committed = s
            .commit_locked(&project(), &thread(), &[r.id.clone()])
            .unwrap();
        assert_eq!(std::fs::read(&committed[0].1).unwrap(), bytes);
        assert_eq!(committed[0].1.extension().unwrap(), "pdf");
        assert!(!committed[0].1.to_string_lossy().contains("report"));
        let message = message_with_paths("", &[committed[0].1.clone()]);
        assert_eq!(message.matches("[Attached files").count(), 1);
        assert!(!message.contains("base64"));
        drop(s);
        let mut restarted = AttachmentService::new(root);
        assert_eq!(
            restarted
                .commit_locked(&project(), &thread(), &[r.id])
                .unwrap(),
            committed
        );
    }

    #[test]
    fn canonical_base64_sequential_offsets_and_declared_length_are_enforced() {
        let (_temp, mut s) = service();
        let id = begin(&mut s, 2).unwrap();
        assert!(s.append_base64(&id, 0, "YQ").is_err());
        assert!(s.append_base64(&id, 0, "YR==").is_err());
        assert!(s.append_base64(&id, 0, "YQ==\n").is_err());
        assert!(s.append(&id, 1, b"a").is_err());
        assert_eq!(s.append_base64(&id, 0, "YQ==").unwrap(), 1);
        assert!(s.finish(&id).is_err());
        assert!(s.append(&id, 1, b"bc").is_err());
        assert_eq!(s.append(&id, 1, b"b").unwrap(), 2);
        assert_eq!(s.finish(&id).unwrap().byte_length, 2);
        assert!(s.append(&id, 2, b"").is_err());
    }

    #[test]
    fn file_and_chunk_byte_boundaries_include_the_exact_limit() {
        let (_temp, mut s) = service();
        assert!(begin(&mut s, FILE_BYTES + 1).is_err());
        let id = begin(&mut s, FILE_BYTES).unwrap();
        assert!(s.append(&id, 0, &vec![0; CHUNK_BYTES + 1]).is_err());
        for offset in (0..FILE_BYTES).step_by(CHUNK_BYTES) {
            assert_eq!(
                s.append(&id, offset, &vec![0; CHUNK_BYTES]).unwrap(),
                offset + CHUNK_BYTES as u64
            );
        }
        assert_eq!(s.finish(&id).unwrap().byte_length, FILE_BYTES);
    }

    #[test]
    fn pending_count_and_declared_byte_reservations_are_independent() {
        let (_temp, mut s) = service();
        for _ in 0..THREAD_PENDING_COUNT {
            begin(&mut s, 0).unwrap();
        }
        assert!(begin(&mut s, 0).is_err());
        let ids = s.uploads.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            s.discard(&AttachmentId::new(id).unwrap()).unwrap();
        }
        for _ in 0..THREAD_PENDING_BYTES / FILE_BYTES {
            begin(&mut s, FILE_BYTES).unwrap();
        }
        assert!(begin(&mut s, 1).is_err());
        assert!(begin(&mut s, 0).is_ok());
    }

    #[test]
    fn unsafe_display_names_and_cross_thread_references_are_refused() {
        let (_temp, mut s) = service();
        for name in ["", "../x", "a/b", "a\\b", "a\nb", &"a".repeat(256)] {
            assert!(
                s.begin(
                    project(),
                    thread(),
                    "draft".into(),
                    name.into(),
                    String::new(),
                    0
                )
                .is_err()
            );
        }
        let r = s
            .upload_bytes(
                project(),
                thread(),
                "draft".into(),
                "broken.png".into(),
                "image/png".into(),
                b"not an image",
            )
            .unwrap();
        let other = ThreadId::new("another").unwrap();
        let root = s.root.clone();
        let _lock = root.lock_thread(other.as_str()).unwrap();
        assert!(s.commit_locked(&project(), &other, &[r.id]).is_err());
    }
}
