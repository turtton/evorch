use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{
        page::{
            EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
            StartScreencastParams,
        },
        target::{CreateTargetParams, WindowState},
    },
};
use event_bus::event::diagnostic_codes;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{BrowserAction, BrowserError, BrowserReport, diagnostics};

pub(super) struct Channels {
    pub commands: mpsc::Receiver<BrowserAction>,
    pub frames: watch::Sender<Option<image::RgbaImage>>,
    pub shutdown: oneshot::Receiver<()>,
    pub reports: mpsc::Sender<BrowserReport>,
}

pub(super) async fn run(
    bus: Arc<event_bus::EventBus>,
    headful: bool,
    mut channels: Channels,
) -> Result<(), BrowserError> {
    let profile = tempfile::tempdir()?;
    let (mut browser, mut handler) =
        Browser::launch(launch_config(profile.path(), headful)?).await?;
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        while let Some(result) = handler.next().await {
            result?;
        }
        Ok::<_, chromiumoxide::error::CdpError>(())
    });
    diagnostics::emit(
        &bus,
        diagnostic_codes::BROWSER_SESSION,
        if headful { "headful" } else { "headless" },
        false,
    );
    let result = tokio::select! {
        _ = &mut channels.shutdown => Ok(()),
        result = stream(&browser, headful, (&bus, &mut channels.commands, &channels.frames, &channels.reports)) => result,
    };
    let closed = browser.close().await;
    if let Err(error) = closed {
        diagnostics::emit(
            &bus,
            diagnostic_codes::BROWSER_CLOSE_ERROR,
            &error.to_string(),
            true,
        );
    }
    let _ = browser.wait().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    diagnostics::emit(
        &bus,
        diagnostic_codes::BROWSER_STOPPED,
        "session closed",
        false,
    );
    result
}

/// chromiumoxide prefixes switches with `--` itself; a key that already has
/// one reaches Chromium as `----key` and is ignored.
pub(super) fn launch_config(
    profile: &std::path::Path,
    headful: bool,
) -> Result<BrowserConfig, BrowserError> {
    let mut config = BrowserConfig::builder()
        .new_headless_mode()
        .user_data_dir(profile)
        .window_size(1280, 720)
        .launch_timeout(Duration::from_secs(20))
        .request_timeout(Duration::from_secs(15))
        .arg("no-startup-window")
        .arg("no-first-run");
    if headful {
        config = config.with_head().arg("start-minimized");
    }
    config.build().map_err(BrowserError::Configuration)
}

async fn stream(
    browser: &Browser,
    headful: bool,
    channels: (
        &event_bus::EventBus,
        &mut mpsc::Receiver<BrowserAction>,
        &watch::Sender<Option<image::RgbaImage>>,
        &mpsc::Sender<BrowserReport>,
    ),
) -> Result<(), BrowserError> {
    let (bus, commands, frames, reports) = channels;
    let mut target = CreateTargetParams::new("about:blank");
    target.background = Some(headful);
    target.new_window = Some(true);
    if headful {
        target.new_window = Some(true);
        target.window_state = Some(WindowState::Minimized);
    }
    let page = browser.new_page(target).await?;
    let mut events = page.event_listener::<EventScreencastFrame>().await?;
    page.execute(
        StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(60)
            .max_width(1280)
            .max_height(720)
            .every_nth_frame(1)
            .build(),
    )
    .await?;
    loop {
        tokio::select! {
            action = commands.recv() => match action {
                Some(action) => {
                    let action_name = match &action {
                        BrowserAction::Navigate(_) => "navigate",
                        BrowserAction::Click(_) => "click",
                    };
                    let report = match diagnostics::perform(&page, action, bus).await {
                        Ok(report) => report,
                        Err(error) => {
                            diagnostics::emit(bus, diagnostic_codes::BROWSER_ACTION_ERROR, &error.to_string(), true);
                            BrowserReport {
                                action: action_name.into(),
                                error: Some(error.to_string()),
                                removed: String::new(),
                                inserted: String::new(),
                            }
                        }
                    };
                    if reports.send(report).await.is_err() { return Ok(()); }
                }
                None => return Ok(()),
            },
            frame = events.next() => match frame {
                Some(frame) => {
                    let image = decode_jpeg(&frame.data, [1280, 720])?;
                    frames.send_replace(Some(image));
                    acknowledge(&page, frame.session_id).await?;
                }
                None => return Err(BrowserError::Unavailable),
            },
        }
    }
}

async fn acknowledge(page: &Page, session_id: i64) -> Result<(), BrowserError> {
    page.execute(ScreencastFrameAckParams::new(session_id))
        .await?;
    Ok(())
}

pub(crate) fn decode_jpeg(
    data: impl AsRef<[u8]>,
    max: [u32; 2],
) -> Result<image::RgbaImage, BrowserError> {
    let jpeg = STANDARD.decode(data)?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max[0]);
    limits.max_image_height = Some(max[1]);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?.into_rgba8())
}
