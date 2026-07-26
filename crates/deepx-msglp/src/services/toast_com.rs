//! Windows COM Activation Callback for interactive toast notifications.
//!
//! Desktop (non-packaged) apps cannot use `ToastNotification::Activated`.
//! Instead, the system invokes an `INotificationActivationCallback` COM
//! object registered via `CoRegisterClassObject` + registry entries.
#![allow(unsafe_op_in_unsafe_fn)]
//!
//! ## Flow
//! 1. `init()` — register COM class factory, write registry keys.
//! 2. `push_pending(id, tx)` — store a reply channel before showing a toast.
//! 3. User clicks toast action → system calls `Activate(appId, args, data)`
//! 4. `Activate` extracts toast id from `args`, looks up `tx`, sends input.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{S_OK, WIN32_ERROR};
use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_CREATE_KEY_DISPOSITION, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW,
};
use windows::core::{GUID, HRESULT, HSTRING, PCWSTR};

// ═══════════════════════════════════════════════════════
// Raw FFI for CoRegisterClassObject (avoids IntoParam<IUnknown>)
// ═══════════════════════════════════════════════════════

unsafe extern "system" {
    #[link_name = "CoRegisterClassObject"]
    fn raw_CoRegisterClassObject(
        rclsid: *const GUID,
        pUnk: *const std::ffi::c_void,
        dwClsContext: u32,
        flags: u32,
        lpdwRegister: *mut u32,
    ) -> HRESULT;
}

// ═══════════════════════════════════════════════════════
// GUIDs
// ═══════════════════════════════════════════════════════

/// CLSID for our INotificationActivationCallback implementation.
const ACTIVATOR_CLSID: GUID = GUID::from_values(
    0x7E6D8F1A,
    0x23B4,
    0x4A9C,
    [0x8F, 0x3D, 0x12, 0xAB, 0xCD, 0xEF, 0x56, 0x78],
);

const IID_IUNKNOWN: GUID = GUID::from_values(
    0x00000000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

const IID_ICLASSFACTORY: GUID = GUID::from_values(
    0x00000001,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

const IID_INOTIFICATION_ACTIVATION_CALLBACK: GUID = GUID::from_values(
    0x53E31837,
    0x6600,
    0x4A81,
    [0x93, 0x95, 0x75, 0xCF, 0xFE, 0x74, 0x6F, 0x94],
);

// ═══════════════════════════════════════════════════════
// NOTIFICATION_USER_INPUT_DATA
// ═══════════════════════════════════════════════════════

#[repr(C)]
#[derive(Clone, Copy)]
struct NotificationUserInputData {
    key: PCWSTR,
    value: PCWSTR,
}

// ═══════════════════════════════════════════════════════
// INotificationActivationCallback  vtable + impl
// ═══════════════════════════════════════════════════════

type ComVoid = *mut std::ffi::c_void;

#[repr(C)]
struct INotificationActivationCallbackVtbl {
    query_interface: unsafe extern "system" fn(ComVoid, *const GUID, *mut ComVoid) -> HRESULT,
    add_ref: unsafe extern "system" fn(ComVoid) -> u32,
    release: unsafe extern "system" fn(ComVoid) -> u32,
    activate: unsafe extern "system" fn(
        ComVoid,
        app_user_model_id: PCWSTR,
        invoked_args: PCWSTR,
        data: *const NotificationUserInputData,
        data_count: u32,
    ) -> HRESULT,
}

struct ToastActivator {
    #[allow(dead_code)]
    vtbl: *const INotificationActivationCallbackVtbl,
    ref_count: u32,
}

static ACTIVATOR_VTBL: INotificationActivationCallbackVtbl = INotificationActivationCallbackVtbl {
    query_interface: activator_query_interface,
    add_ref: activator_add_ref,
    release: activator_release,
    activate: activator_activate,
};

unsafe extern "system" fn activator_query_interface(
    this: ComVoid,
    riid: *const GUID,
    ppv: *mut ComVoid,
) -> HRESULT {
    if ppv.is_null() {
        return HRESULT::from_win32(/* E_POINTER */ 0x80004003u32 as _);
    }
    *ppv = std::ptr::null_mut();
    let iid = &*riid;
    if *iid == IID_INOTIFICATION_ACTIVATION_CALLBACK || *iid == IID_IUNKNOWN {
        *ppv = this;
        activator_add_ref(this);
        S_OK
    } else {
        HRESULT::from_win32(/* E_NOINTERFACE */ 0x80004002u32 as _)
    }
}

unsafe extern "system" fn activator_add_ref(this: ComVoid) -> u32 {
    let obj = &mut *(this as *mut ToastActivator);
    obj.ref_count += 1;
    obj.ref_count
}

unsafe extern "system" fn activator_release(this: ComVoid) -> u32 {
    let obj = &mut *(this as *mut ToastActivator);
    obj.ref_count = obj.ref_count.saturating_sub(1);
    obj.ref_count
}

unsafe extern "system" fn activator_activate(
    _this: ComVoid,
    _app_user_model_id: PCWSTR,
    invoked_args: PCWSTR,
    data: *const NotificationUserInputData,
    data_count: u32,
) -> HRESULT {
    // Extract the toast id from invoked_args.
    let toast_id = match invoked_args.to_string() {
        Ok(s) => s,
        Err(_) => return HRESULT::from_win32(/* E_FAIL */ 0x80004005u32 as _),
    };

    // Extract the "reply" input value.
    let mut reply_text = None;
    for i in 0..data_count as usize {
        let item = &*data.add(i);
        if let Ok(key) = item.key.to_string() {
            if key == "reply" {
                reply_text = item.value.to_string().ok();
                break;
            }
        }
    }

    // Send the reply through the pending channel.
    let mut pending = pending_map().lock().unwrap();
    if let Some(tx) = pending.remove(&toast_id) {
        let _ = tx.send(reply_text);
    }

    S_OK
}

// ═══════════════════════════════════════════════════════
// Global pending reply map
// ═══════════════════════════════════════════════════════

static PENDING: OnceLock<Mutex<HashMap<String, mpsc::Sender<Option<String>>>>> = OnceLock::new();

fn pending_map() -> &'static Mutex<HashMap<String, mpsc::Sender<Option<String>>>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Store a reply channel keyed by a unique toast id.
pub fn push_pending(id: String, tx: mpsc::Sender<Option<String>>) {
    pending_map().lock().unwrap().insert(id, tx);
}

/// Remove and return a reply channel (e.g. on error before showing toast).
pub fn take_pending(id: &str) -> Option<mpsc::Sender<Option<String>>> {
    pending_map().lock().unwrap().remove(id)
}

// ═══════════════════════════════════════════════════════
// IClassFactory  vtable + impl
// ═══════════════════════════════════════════════════════

#[repr(C)]
struct IClassFactoryVtbl {
    query_interface: unsafe extern "system" fn(ComVoid, *const GUID, *mut ComVoid) -> HRESULT,
    add_ref: unsafe extern "system" fn(ComVoid) -> u32,
    release: unsafe extern "system" fn(ComVoid) -> u32,
    create_instance: unsafe extern "system" fn(
        ComVoid,
        *const std::ffi::c_void,
        *const GUID,
        *mut ComVoid,
    ) -> HRESULT,
    lock_server: unsafe extern "system" fn(ComVoid, bool) -> HRESULT,
}

struct ClassFactory {
    #[allow(dead_code)]
    vtbl: *const IClassFactoryVtbl,
    ref_count: u32,
}

static FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    query_interface: factory_query_interface,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};

unsafe extern "system" fn factory_query_interface(
    this: ComVoid,
    riid: *const GUID,
    ppv: *mut ComVoid,
) -> HRESULT {
    if ppv.is_null() {
        return HRESULT::from_win32(0x80004003u32 as _);
    }
    *ppv = std::ptr::null_mut();
    let iid = &*riid;
    if *iid == IID_ICLASSFACTORY || *iid == IID_IUNKNOWN {
        *ppv = this;
        factory_add_ref(this);
        S_OK
    } else {
        HRESULT::from_win32(0x80004002u32 as _)
    }
}

unsafe extern "system" fn factory_add_ref(this: ComVoid) -> u32 {
    let obj = &mut *(this as *mut ClassFactory);
    obj.ref_count += 1;
    obj.ref_count
}

unsafe extern "system" fn factory_release(this: ComVoid) -> u32 {
    let obj = &mut *(this as *mut ClassFactory);
    obj.ref_count = obj.ref_count.saturating_sub(1);
    obj.ref_count
}

unsafe extern "system" fn factory_create_instance(
    _this: ComVoid,
    _outer: *const std::ffi::c_void,
    riid: *const GUID,
    ppv: *mut ComVoid,
) -> HRESULT {
    if ppv.is_null() {
        return HRESULT::from_win32(0x80004003u32 as _);
    }
    *ppv = std::ptr::null_mut();
    let iid = &*riid;
    if *iid == IID_INOTIFICATION_ACTIVATION_CALLBACK || *iid == IID_IUNKNOWN {
        let activator = Box::into_raw(Box::new(ToastActivator {
            vtbl: &ACTIVATOR_VTBL,
            ref_count: 1,
        }));
        *ppv = activator as ComVoid;
        S_OK
    } else {
        HRESULT::from_win32(0x80004002u32 as _)
    }
}

unsafe extern "system" fn factory_lock_server(_this: ComVoid, _lock: bool) -> HRESULT {
    S_OK
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

static REGISTERED: OnceLock<Result<(), HRESULT>> = OnceLock::new();

/// Initialise COM activation callback.  Safe to call multiple times; only
/// the first call actually registers.  Must be called on a COM-initialised
/// thread (the notification thread satisfies this).
pub fn init() {
    REGISTERED.get_or_init(|| {
        let hr = try_register();
        if let Err(e) = &hr {
            log::error!("Toast COM activator registration failed: {e:?}");
        } else {
            log::info!("Toast COM activator registered OK");
        }
        hr
    });
}

fn try_register() -> Result<(), HRESULT> {
    // 1. Write registry entries (best-effort; may lack permissions).
    write_registry();

    // 2. Register class factory with COM.
    let factory = Box::new(ClassFactory {
        vtbl: &FACTORY_VTBL,
        ref_count: 1,
    });
    let factory_ptr: ComVoid = Box::into_raw(factory) as _;

    let mut cookie: u32 = 0;
    let hr = unsafe {
        raw_CoRegisterClassObject(
            &ACTIVATOR_CLSID,
            factory_ptr,
            CLSCTX_LOCAL_SERVER.0,
            REGCLS_MULTIPLEUSE.0 as u32,
            &mut cookie,
        )
    };

    if hr != S_OK {
        // Clean up on failure.
        unsafe {
            drop(Box::from_raw(factory_ptr as *mut ClassFactory));
        }
        return Err(hr);
    }
    // cookie is leaked — class factory lives for process lifetime.
    log::debug!("CoRegisterClassObject cookie={cookie}");
    Ok(())
}

fn write_registry() {
    let clsid_str = format!("{{{:?}}}", ACTIVATOR_CLSID);
    let aumid = "DeepX";

    let subkey_path = format!(
        "Software\\Classes\\AppUserModelId\\{}\\NotificationActivationCallback\\{}",
        aumid, clsid_str
    );

    let result: Result<(), WIN32_ERROR> = (|| unsafe {
        let mut hkey: HKEY = std::mem::zeroed();
        let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
        let ret = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(&subkey_path),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            Some(&mut disposition),
        );
        if ret.0 != 0 {
            return Err(ret);
        }

        // Empty default value — the CLSID subkey name itself is the mapping.
        let _ = RegSetValueExW(
            hkey,
            &HSTRING::new(),
            0,
            REG_SZ,
            None, // empty value
        );

        let _ = RegCloseKey(hkey);
        Ok(())
    })();

    if let Err(e) = result {
        log::warn!("Toast COM registry write failed: {e:?}");
    }
}
