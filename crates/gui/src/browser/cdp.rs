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
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use super::{BrowserAction, BrowserError, diagnostics};

pub(super) struct Channels {
    pub commands: mpsc::Receiver<BrowserAction>,
    pub frames: watch::Sender<Option<egui::ColorImage>>,
    pub shutdown: oneshot::Receiver<()>,
}

pub(super) async fn run(
    bus: Arc<event_bus::EventBus>,
    headful: bool,
    mut channels: Channels,
) -> Result<(), BrowserError> {
    let profile = tempfile::tempdir()?;
    let mut config = BrowserConfig::builder()
        .new_headless_mode()
        .user_data_dir(profile.path())
        .window_size(1280, 720)
        .launch_timeout(Duration::from_secs(20))
        .request_timeout(Duration::from_secs(15))
        .arg("--no-startup-window")
        .arg("--no-first-run");
    if headful {
        config = config.with_head().arg("--start-minimized");
    }
    let config = config.build().map_err(BrowserError::Configuration)?;
    let (mut browser, mut handler) = Browser::launch(config).await?;
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        while let Some(result) = handler.next().await {
            result?;
        }
        Ok::<_, chromiumoxide::error::CdpError>(())
    });
    diagnostics::emit(
        &bus,
        "browser.session",
        if headful { "headful" } else { "headless" },
        false,
    );
    let result = tokio::select! {
        _ = &mut channels.shutdown => Ok(()),
        result = stream(&browser, headful, (&bus, &mut channels.commands, &channels.frames)) => result,
    };
    let closed = browser.close().await;
    if let Err(error) = closed {
        diagnostics::emit(&bus, "browser.close_error", &error.to_string(), true);
    }
    let _ = browser.wait().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    diagnostics::emit(&bus, "browser.stopped", "session closed", false);
    result
}

async fn stream(
    browser: &Browser,
    headful: bool,
    channels: (
        &event_bus::EventBus,
        &mut mpsc::Receiver<BrowserAction>,
        &watch::Sender<Option<egui::ColorImage>>,
    ),
) -> Result<(), BrowserError> {
    let (bus, commands, frames) = channels;
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
                    if let Err(error) = diagnostics::perform(&page, action, bus).await {
                        diagnostics::emit(bus, "browser.action_error", &error.to_string(), true);
                    }
                }
                None => return Ok(()),
            },
            frame = events.next() => match frame {
                Some(frame) => {
                    let image = decode(&frame.data)?;
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

fn decode(data: impl AsRef<[u8]>) -> Result<egui::ColorImage, BrowserError> {
    let jpeg = STANDARD.decode(data)?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1280);
    limits.max_image_height = Some(720);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.into_rgba8();
    let width =
        usize::try_from(image.width()).map_err(|e| BrowserError::Configuration(e.to_string()))?;
    let height =
        usize::try_from(image.height()).map_err(|e| BrowserError::Configuration(e.to_string()))?;
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        image.as_raw(),
    ))
}
