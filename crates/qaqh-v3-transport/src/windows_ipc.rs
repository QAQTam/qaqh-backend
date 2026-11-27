use std::ffi::{OsStr, OsString};
use std::io;
use std::ptr::{self, NonNull};
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) struct PlatformListener {
    listener: Option<NamedPipeServer>,
    name: OsString,
    failure: Option<String>,
}

pub(super) async fn bind_platform(path: &std::path::Path) -> io::Result<PlatformListener> {
    Ok(PlatformListener {
        listener: Some(create_server(path.as_os_str(), true)?),
        name: path.as_os_str().to_owned(),
        failure: None,
    })
}

pub(super) async fn connect_platform(path: &std::path::Path) -> io::Result<NamedPipeClient> {
    let mut attempts = 0u32;
    let mut delay = Duration::from_millis(10);
    loop {
        match ClientOptions::new().open(path.as_os_str()) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                attempts += 1;
                if attempts >= 32 {
                    return Err(error);
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

impl PlatformListener {
    pub(super) async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        if let Some(failure) = &self.failure {
            return Err(io::Error::other(failure.clone()));
        }
        self.listener
            .as_mut()
            .expect("named pipe listener is initialized")
            .connect()
            .await?;
        let connected = self
            .listener
            .take()
            .expect("connected named pipe instance is present");
        match create_server(&self.name, false) {
            Ok(next) => self.listener = Some(next),
            Err(error) => {
                let detail = format!("failed to pre-create next named pipe instance: {error}");
                log::error!("{detail}");
                self.failure = Some(detail);
            }
        }
        Ok(connected)
    }
}

struct LocalSecurityDescriptor(NonNull<std::ffi::c_void>);

impl LocalSecurityDescriptor {
    fn owner_only() -> io::Result<Self> {
        let sddl = owner_only_sddl()?;
        let mut descriptor = ptr::null_mut();
        // SAFETY: the SDDL buffer is NUL-terminated and the output pointer is valid.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor = NonNull::new(descriptor).ok_or_else(|| {
            io::Error::other("ConvertStringSecurityDescriptorToSecurityDescriptorW returned null")
        })?;
        Ok(Self(descriptor))
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the allocation comes from ConvertStringSecurityDescriptorToSecurityDescriptorW
        // and this guard is its sole owner.
        let leaked = unsafe { LocalFree(self.0.as_ptr()) };
        debug_assert!(leaked.is_null());
    }
}

struct ProcessToken(HANDLE);

impl ProcessToken {
    fn current() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle and the output pointer is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if token.is_null() {
            return Err(io::Error::other("OpenProcessToken returned a null token"));
        }
        Ok(Self(token))
    }
}

impl Drop for ProcessToken {
    fn drop(&mut self) {
        // SAFETY: this guard owns the handle returned by OpenProcessToken.
        let closed = unsafe { CloseHandle(self.0) };
        debug_assert_ne!(closed, 0);
    }
}

fn owner_only_sddl() -> io::Result<Vec<u16>> {
    let sid = current_user_sid()?;
    let mut sddl: Vec<u16> = "D:P(A;;GA;;;".encode_utf16().collect();
    sddl.extend_from_slice(&sid);
    sddl.extend(")".encode_utf16());
    sddl.push(0);
    Ok(sddl)
}

fn current_user_sid() -> io::Result<Vec<u16>> {
    let token = ProcessToken::current()?;
    let mut required = 0u32;
    // SAFETY: the first call only queries the required buffer size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut storage = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
    let buffer_len = u32::try_from(storage.len() * size_of::<usize>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "token buffer is too large"))?;
    // SAFETY: storage is large enough for TOKEN_USER and remains alive for the call.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            buffer_len,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: GetTokenInformation filled a TOKEN_USER at the start of storage.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut string = ptr::null_mut();
    // SAFETY: user.User.Sid is a valid process-token SID and the output pointer is valid.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if string.is_null() {
        return Err(io::Error::other(
            "ConvertSidToStringSidW returned a null string",
        ));
    }

    let mut len = 0usize;
    // SAFETY: the converted SID string is NUL-terminated.
    while unsafe { *string.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: string points to len initialized UTF-16 code units.
    let sid = unsafe { std::slice::from_raw_parts(string, len) }.to_vec();
    // SAFETY: the string allocation comes from ConvertSidToStringSidW.
    let leaked = unsafe { LocalFree(string.cast()) };
    debug_assert!(leaked.is_null());
    Ok(sid)
}

fn create_server(name: &OsStr, first_instance: bool) -> io::Result<NamedPipeServer> {
    let descriptor = LocalSecurityDescriptor::owner_only()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.as_ptr(),
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true);
    // SAFETY: attributes and its self-relative descriptor stay alive for this synchronous call;
    // CreateNamedPipeW copies the descriptor into the pipe object.
    unsafe {
        options
            .create_with_security_attributes_raw(name, ptr::addr_of!(attributes).cast_mut().cast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MuxTransport, bind_local_ipc, connect_local_ipc};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn pipe_path(tag: &str) -> OsString {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        OsString::from(format!(
            r"\\.\pipe\qaqh-v3-{tag}-{}-{seq}",
            std::process::id()
        ))
    }

    #[test]
    fn owner_only_sddl_uses_current_user_sid() {
        let sddl = owner_only_sddl().expect("current user SDDL");
        let len = sddl
            .iter()
            .position(|unit| *unit == 0)
            .expect("NUL terminator");
        let sddl = String::from_utf16(&sddl[..len]).expect("UTF-16 SDDL");
        assert!(sddl.starts_with("D:P(A;;GA;;;S-1-"), "{sddl}");
        assert!(!sddl.contains(";;;OW)"), "{sddl}");
    }

    #[tokio::test]
    async fn listener_keeps_accepting_after_precreating_instances() {
        let pipe_path = pipe_path("instances");
        let path = Path::new(&pipe_path);
        let mut listener = bind_local_ipc(path).await.expect("bind");
        for _ in 0..2 {
            let _client = connect_local_ipc(path).await.expect("connect");
            let _server = listener.accept().await.expect("accept");
        }
    }

    #[tokio::test]
    async fn owner_only_pipe_supports_yamux_round_trip() {
        let pipe_path = pipe_path("roundtrip");
        let path = Path::new(&pipe_path);
        let mut listener = bind_local_ipc(path).await.expect("bind");
        let client = connect_local_ipc(path).await.expect("connect");
        let server = listener.accept().await.expect("accept");
        let mut client_stream = client.open_bi().await.expect("client stream");
        client_stream.write_all(b"owner-only").await.expect("write");
        let mut server_stream = server.accept_bi().await.expect("server stream");
        let mut bytes = [0u8; 10];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server_stream.read_exact(&mut bytes),
        )
        .await
        .expect("read timeout")
        .expect("read");
        assert_eq!(&bytes, b"owner-only");
    }
}
