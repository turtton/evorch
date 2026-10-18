use std::sync::{Arc, Mutex};

use cef::*;

use crate::offline::{
    ARTIFACT_HOST, ARTIFACT_ORIGIN, ArtifactBundle, CONTENT_SECURITY_POLICY, artifact_path,
    is_allowed_url,
};
use crate::runtime::{SendCell, on_ui};

#[derive(Default)]
pub struct NetLog {
    pub served: Mutex<Vec<String>>,
    pub blocked: Mutex<Vec<String>>,
}

/// An isolated in-memory request context that serves one [`ArtifactBundle`]
/// from [`ARTIFACT_ORIGIN`] and cancels every other request.
pub struct ArtifactContext {
    pub(crate) context: SendCell<RequestContext>,
    pub log: Arc<NetLog>,
}

impl ArtifactContext {
    pub fn create(bundle: ArtifactBundle) -> Result<Self, String> {
        let log = Arc::new(NetLog::default());
        let handler_log = Arc::clone(&log);
        let bundle = Arc::new(bundle);
        let context = on_ui(move || {
            let mut handler = ArtifactContextHandler::new(Arc::clone(&handler_log));
            let context = request_context_create_context(
                Some(&RequestContextSettings::default()),
                Some(&mut handler),
            )?;
            let mut factory = ArtifactSchemeFactory::new(bundle, handler_log);
            let registered = context.register_scheme_handler_factory(
                Some(&CefString::from("https")),
                Some(&CefString::from(ARTIFACT_HOST)),
                Some(&mut factory),
            );
            (registered == 1).then(|| SendCell(context))
        })
        .flatten()
        .ok_or("failed to create the artifact request context")?;
        Ok(Self { context, log })
    }

    pub fn url(path: &str) -> String {
        format!("{ARTIFACT_ORIGIN}/{}", path.trim_start_matches('/'))
    }
}

fn request_url(request: Option<&mut Request>) -> String {
    request
        .map(|request| CefString::from(&request.url()).to_string())
        .unwrap_or_default()
}

wrap_request_context_handler! {
    struct ArtifactContextHandler {
        log: Arc<NetLog>,
    }

    impl RequestContextHandler {
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: i32,
            _is_download: i32,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut i32>,
        ) -> Option<ResourceRequestHandler> {
            Some(BlockingResourceHandler::new(Arc::clone(&self.log)))
        }
    }
}

wrap_resource_request_handler! {
    struct BlockingResourceHandler {
        log: Arc<NetLog>,
    }

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let url = request_url(request);
            if is_allowed_url(&url) {
                ReturnValue::CONTINUE
            } else {
                self.log.blocked.lock().unwrap_or_else(|e| e.into_inner()).push(url);
                ReturnValue::CANCEL
            }
        }

        fn on_protocol_execution(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            allow_os_execution: Option<&mut i32>,
        ) {
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
        }
    }
}

wrap_scheme_handler_factory! {
    struct ArtifactSchemeFactory {
        bundle: Arc<ArtifactBundle>,
        log: Arc<NetLog>,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let url = request_url(request);
            let file = artifact_path(&url).and_then(|path| self.bundle.get(path));
            self.log.served.lock().unwrap_or_else(|e| e.into_inner()).push(url);
            let (status, mime, body) = match file {
                Some((mime, body)) => (200, mime.to_owned(), body),
                None => (404, "text/plain".to_owned(), Arc::new(b"not found".to_vec())),
            };
            Some(StaticResource::new(
                Arc::new(Head { status, mime }),
                body,
                Arc::new(Mutex::new(0)),
            ))
        }
    }
}

struct Head {
    status: i32,
    mime: String,
}

wrap_resource_handler! {
    struct StaticResource {
        head: Arc<Head>,
        body: Arc<Vec<u8>>,
        offset: Arc<Mutex<usize>>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut i32>,
            _callback: Option<&mut Callback>,
        ) -> i32 {
            if let Some(handle) = handle_request {
                *handle = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(response) = response {
                let header = |name: &str, value: &str| {
                    response.set_header_by_name(
                        Some(&CefString::from(name)),
                        Some(&CefString::from(value)),
                        1,
                    );
                };
                header("Cache-Control", "no-store");
                header("Content-Security-Policy", CONTENT_SECURITY_POLICY);
                response.set_status(self.head.status);
                response.set_mime_type(Some(&CefString::from(self.head.mime.as_str())));
                response.set_charset(Some(&CefString::from("utf-8")));
            }
            if let Some(length) = response_length {
                *length = self.body.len() as i64;
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: i32,
            bytes_read: Option<&mut i32>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> i32 {
            let mut offset = self.offset.lock().unwrap_or_else(|e| e.into_inner());
            let count = (self.body.len() - *offset).min(bytes_to_read.max(0) as usize);
            if let Some(read) = bytes_read {
                *read = count as i32;
            }
            if count == 0 || data_out.is_null() {
                return 0;
            }
            // SAFETY: CEF provides `data_out` with room for `bytes_to_read` bytes,
            // and `count` never exceeds it or the remaining body.
            unsafe { std::ptr::copy_nonoverlapping(self.body[*offset..].as_ptr(), data_out, count) };
            *offset += count;
            1
        }
    }
}
