//! Desktop notification system using Windows toast notifications.
//!
//! Uses a dedicated persistent thread to keep COM initialized across the
//! process lifetime, avoiding FactoryCache use-after-free that occurs
//! when transient threads initialize COM and exit, tearing down the STA.

use std::sync::mpsc;

/// Message sent to the dedicated notification thread.
pub enum NotifyMessage {
    /// Simple one-way toast.
    Toast(String),
    /// Toast with text input; response sent via the channel.
    ToastWithInput {
        body: String,
        reply_tx: mpsc::Sender<Option<String>>,
    },
}

/// Dedicated notification thread that keeps COM initialized across the
/// process lifetime, avoiding FactoryCache use-after-free that occurs
/// when transient threads initialize COM and exit, tearing down the STA.
pub(crate) struct NotificationThread {
    tx: mpsc::Sender<NotifyMessage>,
    _thread: std::thread::JoinHandle<()>,
}

impl NotificationThread {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<NotifyMessage>();
        let thread = std::thread::Builder::new()
            .name("deepx-notify".into())
            .spawn(move || {
                #[cfg(windows)]
                let winrt_initialized = std::env::var_os("DEEPX_NATIVE_NOTIFICATIONS")
                    .is_some_and(|value| value == "1")
                    && unsafe {
                        windows::Win32::System::WinRT::RoInitialize(
                            windows::Win32::System::WinRT::RO_INIT_SINGLETHREADED,
                        )
                        .map(|_| true)
                        .unwrap_or_else(|error| {
                            log::warn!(
                                "Windows Runtime initialization failed; notifications disabled: {error}"
                            );
                            false
                        })
                    };
                'outer: loop {
                    let mut got_any = false;
                    loop {
                        match rx.try_recv() {
                            Ok(NotifyMessage::Toast(body)) => {
                                got_any = true;
                                #[cfg(windows)]
                                if winrt_initialized {
                                    show_toast_windows(&body);
                                }
                                #[cfg(not(windows))]
                                let _ = &body;
                            }
                            Ok(NotifyMessage::ToastWithInput { body, reply_tx }) => {
                                got_any = true;
                                #[cfg(windows)]
                                if winrt_initialized {
                                    show_toast_with_input_windows(&body, reply_tx);
                                } else {
                                    let _ = reply_tx.send(None);
                                }
                                #[cfg(not(windows))]
                                {
                                    let _ = &body;
                                    let _ = reply_tx.send(None);
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'outer,
                        }
                    }
                    #[cfg(windows)]
                    pump_com_messages();
                    if !got_any {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                #[cfg(windows)]
                if winrt_initialized {
                    unsafe {
                        windows::Win32::System::WinRT::RoUninitialize();
                    }
                }
            })
            .expect("failed to spawn notification thread");
        Self {
            tx,
            _thread: thread,
        }
    }

    /// Send a simple one-way toast notification.
    pub(crate) fn notify(&self, body: String) {
        let _ = self.tx.send(NotifyMessage::Toast(body));
    }

    /// Consume the thread handle and return the sender channel.
    /// Used by the new Loop architecture to integrate with NotifyHandle.
    pub(crate) fn into_sender(self) -> mpsc::Sender<NotifyMessage> {
        self.tx
    }

    /// Send an interactive toast with a text input box.
    #[allow(dead_code)]
    pub(crate) fn notify_input(&self, body: String) -> mpsc::Receiver<Option<String>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let _ = self
            .tx
            .send(NotifyMessage::ToastWithInput { body, reply_tx });
        reply_rx
    }
}

#[cfg(windows)]
fn ensure_aumid() -> &'static str {
    use std::sync::OnceLock;
    static AUMID: OnceLock<String> = OnceLock::new();
    AUMID.get_or_init(|| {
        let our_id = "DeepX";
        unsafe {
            let hr = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                &windows::core::HSTRING::from(our_id),
            );
            log::info!("SetCurrentProcessExplicitAppUserModelID({our_id}) → {hr:?}");
        }
        let ps_id =
            "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";
        log::info!("Using PowerShell AUMID for toast");
        ps_id.to_string()
    })
}

#[cfg(windows)]
fn pump_com_messages() {
    unsafe {
        let mut msg: windows::Win32::UI::WindowsAndMessaging::MSG = std::mem::zeroed();
        loop {
            let has_msg = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut msg,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            );
            if !has_msg.as_bool() {
                break;
            }
            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }
}

#[cfg(windows)]
fn show_toast_with_input_windows(body: &str, reply_tx: mpsc::Sender<Option<String>>) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TOAST_ID: AtomicU64 = AtomicU64::new(1);

    super::toast_com::init();

    let id = format!("deepx:{}", TOAST_ID.fetch_add(1, Ordering::Relaxed));

    super::toast_com::push_pending(id.clone(), reply_tx);

    let escaped = escape_xml(body);
    let xml = format!(
        "<toast duration=\"long\">\
            <visual>\
                <binding template=\"ToastGeneric\">\
                    <text>DeepX</text>\
                    <text>{}</text>\
                </binding>\
            </visual>\
            <actions>\
                <input id=\"reply\" type=\"text\" placeHolderContent=\"Type a reply...\" title=\"Reply\"/>\
                <action content=\"Send\" arguments=\"{}\" hint-inputId=\"reply\" activationType=\"foreground\"/>\
            </actions>\
        </toast>",
        escaped, id
    );

    let doc = match windows::Data::Xml::Dom::XmlDocument::new() {
        Ok(d) => d,
        Err(e) => {
            let _ = super::toast_com::take_pending(&id).and_then(|tx| tx.send(None).ok());
            log::error!("XmlDocument::new failed: {e:?}");
            return;
        }
    };
    if let Err(e) = doc.LoadXml(&windows::core::HSTRING::from(xml.as_str())) {
        let _ = super::toast_com::take_pending(&id).and_then(|tx| tx.send(None).ok());
        log::error!("show_toast_input: LoadXml failed: {e:?}");
        return;
    }
    let toast = match windows::UI::Notifications::ToastNotification::CreateToastNotification(&doc) {
        Ok(t) => t,
        Err(e) => {
            let _ = super::toast_com::take_pending(&id).and_then(|tx| tx.send(None).ok());
            log::error!("CreateToastNotification failed: {e:?}");
            return;
        }
    };

    let aumid = ensure_aumid();
    let notifier =
        match windows::UI::Notifications::ToastNotificationManager::CreateToastNotifierWithId(
            &windows::core::HSTRING::from(aumid),
        ) {
            Ok(n) => n,
            Err(e) => {
                let _ = super::toast_com::take_pending(&id).and_then(|tx| tx.send(None).ok());
                log::error!("CreateToastNotifierWithId({aumid}) failed: {e:?}");
                return;
            }
        };

    if let Err(e) = notifier.Show(&toast) {
        log::error!("show_toast_input: Show failed: {e:?}");
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    log::info!("show_toast_input: success");
}

#[cfg(windows)]
fn show_toast_windows(body: &str) {
    let aumid = ensure_aumid();
    log::info!("show_toast: body_len={} aumid={aumid}", body.len());

    let escaped = escape_xml(body);
    let xml = format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\"><text>DeepX</text><text>{}</text></binding></visual></toast>",
        escaped
    );

    let doc = match windows::Data::Xml::Dom::XmlDocument::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("show_toast: XmlDocument::new failed: {e:?}");
            return;
        }
    };
    if let Err(e) = doc.LoadXml(&windows::core::HSTRING::from(xml.as_str())) {
        log::error!("show_toast: LoadXml failed: {e:?}");
        return;
    }
    let toast = match windows::UI::Notifications::ToastNotification::CreateToastNotification(&doc) {
        Ok(t) => t,
        Err(e) => {
            log::error!("show_toast: CreateToastNotification failed: {e:?}");
            return;
        }
    };

    let notifier =
        match windows::UI::Notifications::ToastNotificationManager::CreateToastNotifierWithId(
            &windows::core::HSTRING::from(aumid),
        ) {
            Ok(n) => n,
            Err(e) => {
                log::error!("show_toast: CreateToastNotifierWithId({aumid}) failed: {e:?}");
                return;
            }
        };

    if let Err(e) = notifier.Show(&toast) {
        log::error!("show_toast: Show failed: {e:?}");
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    log::info!("show_toast: success");
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
