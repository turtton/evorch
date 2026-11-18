use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

use runtime::artifacts::{ArtifactRecord, ArtifactStore};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    namespaces::artifacts::{ArtifactMetadata, ArtifactRef, MAX_CONTENT_CHUNK_BYTES},
};

pub(crate) fn not_found() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::NotFound,
        "artifact is not available",
        RetryAdvice::Never,
    )
}

pub(crate) fn storage_error() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::StorageUnavailable,
        "artifact storage is unavailable",
        RetryAdvice::SameRequest,
    )
}

fn invalid(message: &'static str) -> WorkbenchError {
    WorkbenchError::new(ErrorCode::InvalidRequest, message, RetryAdvice::Never)
}

pub(super) fn supported_type(record: &ArtifactRecord) -> bool {
    matches!(
        (record.media_type.as_str(), record.extension.as_str()),
        ("text/html", "html")
            | ("text/plain", "txt")
            | ("image/png", "png")
            | ("image/jpeg", "jpg" | "jpeg")
            | ("image/gif", "gif")
            | ("image/webp", "webp")
    )
}

pub(crate) fn metadata(reference: ArtifactRef, record: &ArtifactRecord) -> ArtifactMetadata {
    let mut title = String::new();
    for ch in record.title.chars().filter(|c| !c.is_control()) {
        if title.len() + ch.len_utf8() > 1024 {
            break;
        }
        title.push(ch);
    }
    if title.is_empty() {
        title.push_str("Artifact");
    }
    ArtifactMetadata {
        reference,
        title,
        media_type: record.media_type.clone(),
        byte_len: record.byte_len,
    }
}

fn blob(store: &ArtifactStore, record: &ArtifactRecord) -> Result<File, WorkbenchError> {
    // No client paths, metadata-controlled extensions, symlinks, or FIFO waits.
    if !supported_type(record)
        || record.sha256.len() != 64
        || !record.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(not_found());
    }
    let fd = rustix::fs::open(
        store.blob_path(record),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| storage_error())?;
    let file = File::from(fd);
    let info = file.metadata().map_err(|_| storage_error())?;
    if !info.is_file() || info.len() != record.byte_len {
        return Err(storage_error());
    }
    Ok(file)
}

pub(super) fn read_text(
    store: &ArtifactStore,
    record: &ArtifactRecord,
    offset: u64,
    max_bytes: u32,
) -> Result<(String, bool), WorkbenchError> {
    if !matches!(record.media_type.as_str(), "text/html" | "text/plain") {
        return Err(invalid(
            "image artifact content is not available through this API",
        ));
    }
    if max_bytes == 0 || max_bytes as usize > MAX_CONTENT_CHUNK_BYTES || offset > record.byte_len {
        return Err(invalid("invalid artifact content range"));
    }
    let mut file = blob(store, record)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| storage_error())?;
    let mut bytes = Vec::new();
    file.take(u64::from(max_bytes))
        .read_to_end(&mut bytes)
        .map_err(|_| storage_error())?;
    if bytes.len() as u64 != (record.byte_len - offset).min(u64::from(max_bytes)) {
        return Err(storage_error());
    }
    let end = offset
        .checked_add(bytes.len() as u64)
        .ok_or_else(storage_error)?;
    let eof = end == record.byte_len;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if !eof && error.error_len().is_none() && error.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|_| storage_error())?
        }
        Err(_) => {
            return Err(invalid(
                "content range must contain complete UTF-8 characters",
            ));
        }
    };
    Ok((text.to_owned(), eof))
}

/// Only the owned operation worker calls this; no HTML/blob load under the host mutex.
pub(crate) fn preview_html(
    store: &ArtifactStore,
    record: &ArtifactRecord,
) -> Result<Vec<u8>, WorkbenchError> {
    use base64::Engine;
    const MAX_HTML: usize = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    blob(store, record)?
        .take((MAX_HTML + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| storage_error())?;
    if record.byte_len <= MAX_HTML as u64 && bytes.len() as u64 != record.byte_len {
        return Err(storage_error());
    }
    let html = match record.media_type.as_str() {
        "text/html" => {
            std::str::from_utf8(&bytes).map_err(|_| invalid("artifact is not UTF-8 HTML"))?;
            bytes
        }
        "text/plain" => {
            let text =
                std::str::from_utf8(&bytes).map_err(|_| invalid("artifact is not UTF-8 text"))?;
            let mut html = String::from("<!doctype html><meta charset=\"utf-8\"><pre>");
            for ch in text.chars() {
                let escaped = match ch {
                    '&' => Some("&amp;"),
                    '<' => Some("&lt;"),
                    '>' => Some("&gt;"),
                    _ => None,
                };
                if html.len() + escaped.map_or(ch.len_utf8(), str::len) + "</pre>".len() > MAX_HTML
                {
                    return Err(invalid("artifact is too large for preview"));
                }
                if let Some(escaped) = escaped {
                    html.push_str(escaped);
                } else {
                    html.push(ch);
                }
            }
            html.push_str("</pre>");
            html.into_bytes()
        }
        _ => {
            if bytes.len() > MAX_HTML / 4 * 3 - 1024 {
                return Err(invalid("artifact is too large for preview"));
            }
            format!(
                "<!doctype html><img src=\"data:{};base64,{}\">",
                record.media_type,
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
            .into_bytes()
        }
    };
    if html.len() > MAX_HTML {
        return Err(invalid("artifact is too large for preview"));
    }
    Ok(html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(
        root: &Path,
        bytes: &[u8],
        media_type: &str,
        extension: &str,
    ) -> (ArtifactStore, ArtifactRecord) {
        let store = ArtifactStore::new(root).unwrap();
        let record = ArtifactRecord {
            artifact_id: format!("artifact-{}", "a".repeat(32)),
            sha256: "b".repeat(64),
            media_type: media_type.into(),
            extension: extension.into(),
            title: "Artifact".into(),
            caption: None,
            source_path: "/private/source/secret.html".into(),
            byte_len: bytes.len() as u64,
            run_id: "run-content".into(),
            root_run_id: "run-content".into(),
            thread_id: Some("thread-content".into()),
            inlined_assets: Vec::new(),
        };
        std::fs::write(store.blob_path(&record), bytes).unwrap();
        (store, record)
    }

    #[test]
    fn utf8_chunks_use_byte_offsets_without_splitting_characters() {
        let dir = tempfile::tempdir().unwrap();
        let (store, record) = fixture(dir.path(), "a界🙂z".as_bytes(), "text/plain", "txt");
        assert_eq!(
            read_text(&store, &record, 0, 3).unwrap(),
            ("a".into(), false)
        );
        assert_eq!(
            read_text(&store, &record, 1, 3).unwrap(),
            ("界".into(), false)
        );
        assert_eq!(
            read_text(&store, &record, 4, 4).unwrap(),
            ("🙂".into(), false)
        );
        assert_eq!(
            read_text(&store, &record, 8, 4).unwrap(),
            ("z".into(), true)
        );
        assert_eq!(
            read_text(&store, &record, 9, 4).unwrap(),
            (String::new(), true)
        );
        for (offset, limit) in [
            (1, 1),
            (2, 3),
            (5, 4),
            (10, 4),
            (0, 0),
            (0, MAX_CONTENT_CHUNK_BYTES as u32 + 1),
        ] {
            assert!(
                matches!(read_text(&store, &record, offset, limit), Err(error) if error.code == ErrorCode::InvalidRequest)
            );
        }
    }

    #[test]
    fn chunk_budget_is_a_hard_byte_bound_and_images_never_return_content() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; MAX_CONTENT_CHUNK_BYTES + 1];
        let (store, record) = fixture(dir.path(), &bytes, "text/html", "html");
        let (text, eof) = read_text(&store, &record, 0, MAX_CONTENT_CHUNK_BYTES as u32).unwrap();
        assert_eq!(text.as_bytes(), &bytes[..MAX_CONTENT_CHUNK_BYTES]);
        assert!(!eof);
        for (media_type, extension) in [
            ("image/png", "png"),
            ("image/jpeg", "jpg"),
            ("image/gif", "gif"),
            ("image/webp", "webp"),
        ] {
            let (_, record) = fixture(dir.path(), b"private-image-payload", media_type, extension);
            let error = read_text(&store, &record, 0, 1024).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert_eq!(
                error.message,
                "image artifact content is not available through this API"
            );
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("private-image-payload")
            );
        }
    }

    #[test]
    fn saved_html_is_unmodified_but_plain_text_preview_is_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let html = "<img src=\"data:image/png;base64,original-inline-data\">";
        let (store, record) = fixture(dir.path(), html.as_bytes(), "text/html", "html");
        assert_eq!(
            read_text(&store, &record, 0, 1024).unwrap(),
            (html.into(), true)
        );
        assert_eq!(preview_html(&store, &record).unwrap(), html.as_bytes());
        let (store, record) = fixture(dir.path(), b"<script>&</script>", "text/plain", "txt");
        let preview = String::from_utf8(preview_html(&store, &record).unwrap()).unwrap();
        assert!(preview.contains("&lt;script&gt;&amp;&lt;/script&gt;"));
        assert!(!preview.contains("<script>"));
    }

    #[test]
    fn symlinks_bad_extensions_and_changed_blob_lengths_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut record) = fixture(dir.path(), b"allowed", "text/html", "html");
        std::fs::write(store.blob_path(&record), b"changed-size").unwrap();
        assert_eq!(
            read_text(&store, &record, 0, 1024).unwrap_err().code,
            ErrorCode::StorageUnavailable
        );
        let path = store.blob_path(&record);
        std::fs::remove_file(&path).unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, b"secret!").unwrap();
        std::os::unix::fs::symlink(secret, path).unwrap();
        assert_eq!(
            read_text(&store, &record, 0, 1024).unwrap_err().code,
            ErrorCode::StorageUnavailable
        );
        record.extension = "../secret".into();
        assert_eq!(
            read_text(&store, &record, 0, 1024).unwrap_err().code,
            ErrorCode::NotFound
        );
    }
}
