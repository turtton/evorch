use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    path::Path,
};
pub struct Watch {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
    mask: u32,
}
impl Watch {
    pub fn new(path: &Path, mask: u32) -> Self {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        assert!(fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert!(unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } >= 0);
        Self {
            fd: tokio::io::unix::AsyncFd::new(fd).unwrap(),
            mask,
        }
    }
    pub async fn event(&self, name: Option<&str>) {
        loop {
            let mut ready = self.fd.readable().await.unwrap();
            let mut bytes = [0u8; 4096];
            let n = unsafe {
                libc::read(
                    self.fd.get_ref().as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if n < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                ready.clear_ready();
                continue;
            }
            let mut offset = 0;
            while offset < n as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        bytes.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                let start = offset + std::mem::size_of::<libc::inotify_event>();
                let end = start + event.len as usize;
                let filename = bytes[start..end]
                    .split(|&b| b == 0)
                    .next()
                    .unwrap_or_default();
                if event.mask & self.mask != 0 && name.is_none_or(|n| n.as_bytes() == filename) {
                    return;
                }
                offset = end;
            }
        }
    }
}
