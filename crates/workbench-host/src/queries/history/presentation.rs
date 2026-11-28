//! The public view of an artifacts row. Its strings are read from storage as stored (a long
//! or escape-led string is a body token), so this is where they become text: tokens are
//! resolved once, in one short read of their own after the history snapshot is closed, then
//! secrets are masked, control characters removed and the text cut to its public cap.
use super::super::{error, masked};
use storage::history::rows::{
    PRESENTATION_CAPTION_BYTES, PRESENTATION_MEDIA_TYPE_BYTES, PRESENTATION_TITLE_BYTES,
};
use storage::history::{PresentationRow, PresentationText};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    namespaces::artifacts::{
        ArtifactPresentationView, MAX_PRESENTED_ARTIFACTS, PresentedArtifactView,
    },
};

/// Reads the bodies of the given references; the host database and the runtime's reader
/// both give one.
pub(super) type Resolve<'a> = dyn FnMut(
        &[storage::BodyRef],
    ) -> Result<Vec<Result<String, storage::BodyFailure>>, storage::StorageError>
    + 'a;

#[derive(Clone, Copy)]
enum Field {
    Title,
    Caption,
    MediaType,
}

impl Field {
    fn cap(self) -> usize {
        match self {
            Self::Title => PRESENTATION_TITLE_BYTES,
            Self::Caption => PRESENTATION_CAPTION_BYTES,
            Self::MediaType => PRESENTATION_MEDIA_TYPE_BYTES,
        }
    }
}

fn unavailable() -> WorkbenchError {
    error(
        ErrorCode::StorageUnavailable,
        "an artifact presentation cannot be restored",
    )
}

/// Every string the view shows, in the order the view consumes them.
fn texts(row: &PresentationRow) -> Vec<(&PresentationText, Field)> {
    let mut texts = Vec::with_capacity(2 + 3 * row.artifacts.len());
    texts.extend(row.title.iter().map(|text| (text, Field::Title)));
    texts.extend(row.caption.iter().map(|text| (text, Field::Caption)));
    for artifact in &row.artifacts {
        texts.push((&artifact.title, Field::Title));
        texts.extend(artifact.caption.iter().map(|text| (text, Field::Caption)));
        texts.push((&artifact.media_type, Field::MediaType));
    }
    texts
}

fn bounded(chars: impl Iterator<Item = char>, cap: usize) -> String {
    let mut text = String::new();
    for ch in chars {
        if text.len() + ch.len_utf8() > cap {
            break;
        }
        text.push(ch);
    }
    text
}

fn single_line(value: &str, cap: usize) -> String {
    bounded(masked(value).chars().filter(|ch| !ch.is_control()), cap)
}

fn multi_line(value: &str, cap: usize) -> String {
    bounded(
        masked(value)
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\t' | '\n' | '\r')),
        cap,
    )
}

/// The view of a presentation row, or an error when it cannot be shown whole. A body that
/// cannot be read, a token that is not one, a count or a length beyond what `present`
/// produces, or an identifier of the wrong shape fails the row: nothing is dropped.
///
/// At most 26 strings are resolved, none longer than its field's cap, so one row reads
/// less than 48 KiB. Resolved text is final and never read as a token again.
pub(super) fn view(
    row: &PresentationRow,
    resolve: &mut Resolve<'_>,
) -> Result<ArtifactPresentationView, WorkbenchError> {
    if row.artifacts.len() > MAX_PRESENTED_ARTIFACTS {
        return Err(error(
            ErrorCode::LimitExceeded,
            "an artifact presentation holds more artifacts than the protocol allows",
        ));
    }
    let texts = texts(row);
    let mut references = Vec::new();
    for (text, field) in &texts {
        if !text.1 {
            continue;
        }
        let reference = storage::BodyRef::from_token(&text.0).ok_or_else(unavailable)?;
        if reference.byte_len > field.cap() as u64 {
            return Err(error(
                ErrorCode::LimitExceeded,
                "artifact presentation text is longer than the protocol allows",
            ));
        }
        references.push(reference);
    }
    let mut resolved = if references.is_empty() {
        Vec::new()
    } else {
        resolve(&references).map_err(|_| unavailable())?
    }
    .into_iter();
    let mut strings = Vec::with_capacity(texts.len());
    for (text, _) in &texts {
        strings.push(if text.1 {
            resolved
                .next()
                .and_then(Result::ok)
                .ok_or_else(unavailable)?
        } else {
            text.0.clone()
        });
    }
    if resolved.next().is_some() {
        return Err(unavailable());
    }
    let mut strings = strings.into_iter();
    let mut next = |field: Field| strings.next().map(|value| (value, field));
    let heading = |value: Option<(String, Field)>, fallback: &str| {
        value.map(|(value, field)| {
            let text = single_line(&value, field.cap());
            if text.is_empty() {
                fallback.to_owned()
            } else {
                text
            }
        })
    };
    let caption = |value: Option<(String, Field)>| {
        value.map(|(value, field)| multi_line(&value, field.cap()))
    };
    let title = heading(
        row.title.as_ref().and_then(|_| next(Field::Title)),
        "Artifacts",
    );
    let presentation_caption = caption(row.caption.as_ref().and_then(|_| next(Field::Caption)));
    let mut artifacts = Vec::with_capacity(row.artifacts.len());
    for artifact in &row.artifacts {
        let artifact_title = heading(next(Field::Title), "Artifact").ok_or_else(unavailable)?;
        let artifact_caption =
            caption(artifact.caption.as_ref().and_then(|_| next(Field::Caption)));
        let media_type = next(Field::MediaType)
            .map(|(value, field)| single_line(&value, field.cap()))
            .ok_or_else(unavailable)?;
        artifacts.push(PresentedArtifactView {
            artifact_id: artifact.artifact_id.clone(),
            digest: artifact.digest.clone(),
            title: artifact_title,
            caption: artifact_caption,
            media_type: if media_type.is_empty() {
                "application/octet-stream".to_owned()
            } else {
                media_type
            },
        });
    }
    let view = ArtifactPresentationView {
        title,
        caption: presentation_caption,
        artifacts,
    };
    view.validate().map_err(|_| unavailable())?;
    Ok(view)
}
