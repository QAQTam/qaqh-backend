//! ProjFS 承载的 pending-overlay —— M2 RedirectPlane 核心(ADR-0003 裁决)。
//!
//! 语义(与 spec §4.2/§4.3 冻结契约对齐):
//! - [`View::start`]:把 `store`(只读基底,如工作区快照)投影进 `root`(虚拟视图),
//!   沙箱进程在视图内看到合并视图;
//! - 视图内新建/改写/删除 = upper 语义:新建落为根目录真实文件,改写经内核水合
//!   copy-up 就地落盘(store 永不被穿透),删除由内核维护 tombstone;
//! - [`View::diff`]:turn 边界取变更清单(经 `PrjGetOnDiskFileState`,不触发水合);
//! - [`View::merge`]:把变更落回 store;[`View::discard`]:毫秒级丢弃
//!   (改写文件经 `PrjDeleteFile` 还原为占位,重新投影即回原始内容)。
//!
//! 工程事实(ADR-0003,全部实证):根准备两步(Mark + Start);枚举必须按
//! searchExpression 过滤;windows-rs 对 projectedfslib.dll 是硬导入且绑定不全,
//! 故全部函数走 LoadLibrary + GetProcAddress,类型复用 crate 纯定义。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{GUID, PCWSTR, HRESULT};
use windows::Win32::Foundation::{BOOLEAN, FILETIME};
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
    PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_FILE_STATE, PRJ_NOTIFICATION,
    PRJ_NOTIFICATION_FILE_OVERWRITTEN, PRJ_NOTIFICATION_FILE_RENAMED,
    PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFICATION_NEW_FILE_CREATED, PRJ_NOTIFICATION_PRE_DELETE,
    PRJ_NOTIFICATION_PRE_RENAME, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PRJ_NOTIFY_FILE_OVERWRITTEN, PRJ_NOTIFY_FILE_RENAMED, PRJ_NOTIFY_NEW_FILE_CREATED,
    PRJ_NOTIFY_PRE_DELETE, PRJ_NOTIFY_PRE_RENAME, PRJ_NOTIFY_TYPES, PRJ_PLACEHOLDER_INFO,
    PRJ_PLACEHOLDER_VERSION_INFO, PRJ_STARTVIRTUALIZING_OPTIONS, PRJ_UPDATE_TYPES,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const S_OK: HRESULT = HRESULT(0);
const E_FILE_NOT_FOUND: HRESULT = HRESULT(0x8007_0002_u32 as i32);
const E_INSUFFICIENT_BUFFER: HRESULT = HRESULT(0x8007_007A_u32 as i32);
const E_INVALID_ARG: HRESULT = HRESULT(0x8007_0057_u32 as i32);

/// PrjGetOnDiskFileState 位:脏/满 = 有本地改动(相对占位状态)
const STATE_DIRTY_PLACEHOLDER: i32 = 4;
const STATE_FULL: i32 = 8;

/// PrjDeleteFile 允许一切状态被删(脏数据/脏元数据/tombstone/只读)
const UPDATE_ALLOW_ALL: PRJ_UPDATE_TYPES = PRJ_UPDATE_TYPES(1 | 2 | 4 | 32);

// ---------------------------------------------------------------- FFI(动态加载)

type PrjStartVirtualizingFn = unsafe extern "system" fn(
    virtualizationrootpath: PCWSTR,
    callbacks: *const PRJ_CALLBACKS,
    instancecontext: *const core::ffi::c_void,
    options: *const PRJ_STARTVIRTUALIZING_OPTIONS,
    namespacevirtualizationcontext: *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
) -> HRESULT;
type PrjStopVirtualizingFn = unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
type PrjFillDirEntryBufferFn = unsafe extern "system" fn(
    filename: PCWSTR,
    filebasicinfo: *const PRJ_FILE_BASIC_INFO,
    direntrybufferhandle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT;
type PrjWritePlaceholderInfoFn = unsafe extern "system" fn(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    destinationfilename: PCWSTR,
    placeholderinfo: *const PRJ_PLACEHOLDER_INFO,
    placeholderinfosize: u32,
) -> HRESULT;
type PrjWriteFileDataFn = unsafe extern "system" fn(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    datastreamid: *const GUID,
    buffer: *const core::ffi::c_void,
    byteoffset: u64,
    length: u32,
) -> HRESULT;
type PrjMarkDirectoryAsPlaceholderFn = unsafe extern "system" fn(
    rootpathname: PCWSTR,
    targetpathname: PCWSTR,
    versioninfo: *const PRJ_PLACEHOLDER_VERSION_INFO,
    virtualizationinstanceid: *const GUID,
) -> HRESULT;
type PrjFileNameMatchFn =
    unsafe extern "system" fn(filenametocheck: PCWSTR, pattern: PCWSTR) -> BOOLEAN;
type PrjFileNameCompareFn = unsafe extern "system" fn(filename1: PCWSTR, filename2: PCWSTR) -> i32;
type PrjDeleteFileFn = unsafe extern "system" fn(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    destinationfilename: PCWSTR,
    updateflags: PRJ_UPDATE_TYPES,
    failurereason: *mut windows::Win32::Storage::ProjectedFileSystem::PRJ_UPDATE_FAILURE_CAUSES,
) -> HRESULT;
type PrjGetOnDiskFileStateFn =
    unsafe extern "system" fn(destinationfilename: PCWSTR, filestate: *mut PRJ_FILE_STATE)
        -> HRESULT;

#[derive(Clone)]
struct Prj {
    start_virtualizing: PrjStartVirtualizingFn,
    stop_virtualizing: PrjStopVirtualizingFn,
    fill_dir_entry: PrjFillDirEntryBufferFn,
    write_placeholder: PrjWritePlaceholderInfoFn,
    write_file_data: PrjWriteFileDataFn,
    mark_dir_as_placeholder: PrjMarkDirectoryAsPlaceholderFn,
    file_name_match: PrjFileNameMatchFn,
    file_name_compare: PrjFileNameCompareFn,
    delete_file: PrjDeleteFileFn,
    get_on_disk_file_state: PrjGetOnDiskFileStateFn,
    _lib: windows::Win32::Foundation::HMODULE,
}

// HMODULE 只是句柄;全部函数指针为无状态 extern fn → 跨线程安全
unsafe impl Send for Prj {}
unsafe impl Sync for Prj {}

static PRJ: OnceLock<Option<Prj>> = OnceLock::new();

impl Prj {
    /// DLL 缺失(Client-ProjFS 未启用)→ None;进程内缓存一次加载结果。
    fn get() -> Option<&'static Prj> {
        PRJ.get_or_init(|| unsafe { Self::load() }).as_ref()
    }

    unsafe fn load() -> Option<Prj> {
        unsafe {
            let name: Vec<u16> = "projectedfslib.dll".encode_utf16().chain([0]).collect();
            let lib = LoadLibraryW(PCWSTR(name.as_ptr())).ok()?;
            let get = |sym: &str| -> Option<*mut core::ffi::c_void> {
                let bytes = [sym.as_bytes(), &[0]].concat();
                let far = GetProcAddress(lib, windows::core::PCSTR(bytes.as_ptr()));
                far.map(|f| f as *mut core::ffi::c_void)
            };
            macro_rules! need {
                ($sym:literal, $ty:ty) => {
                    std::mem::transmute::<*mut core::ffi::c_void, $ty>(get($sym)?)
                };
            }
            Some(Prj {
                start_virtualizing: need!("PrjStartVirtualizing", PrjStartVirtualizingFn),
                stop_virtualizing: need!("PrjStopVirtualizing", PrjStopVirtualizingFn),
                fill_dir_entry: need!("PrjFillDirEntryBuffer", PrjFillDirEntryBufferFn),
                write_placeholder: need!("PrjWritePlaceholderInfo", PrjWritePlaceholderInfoFn),
                write_file_data: need!("PrjWriteFileData", PrjWriteFileDataFn),
                // windows 0.58 绑定缺此函数;且 PrjMarkDirectoryAsRoot 在本 DLL 无导出
                mark_dir_as_placeholder: need!(
                    "PrjMarkDirectoryAsPlaceholder",
                    PrjMarkDirectoryAsPlaceholderFn
                ),
                file_name_match: need!("PrjFileNameMatch", PrjFileNameMatchFn),
                file_name_compare: need!("PrjFileNameCompare", PrjFileNameCompareFn),
                delete_file: need!("PrjDeleteFile", PrjDeleteFileFn),
                get_on_disk_file_state: need!("PrjGetOnDiskFileState", PrjGetOnDiskFileStateFn),
                _lib: lib,
            })
        }
    }
}

/// ProjFS 承载层可用性(诊断上报;DLL 缺失 = 可选功能未启用)。
pub fn available() -> bool {
    Prj::get().is_some()
}

// ---------------------------------------------------------------- Provider

#[derive(Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    size: i64,
    write_time: i64,
}

struct Session {
    entries: Vec<Entry>,
    cursor: usize,
}

/// 视图通知(观察流,v1 只记录不 veto——强制力在平面一,视图层不做第二套
/// 裁决;治理层按需消费)。通知映射:pre-delete/pre-rename/renamed/
/// overwritten/new-file;写关闭事件(PRJ_NOTIFY_FILE_HANDLE_CLOSED_*)频率
/// 过高,内容变化由 turn 边界 diff 承担。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// 文件将被删除(DACL 放行后才触发)
    PreDelete { path: String },
    /// 文件将被改名(pre-rename;dest = 新名)
    PreRename { path: String, dest: String },
    /// 改名完成(path = 旧名,dest = 新名)
    Renamed { path: String, dest: String },
    /// 既有文件被覆盖写(截断/超写)
    Overwritten { path: String },
    /// 视图内新建文件
    NewFile { path: String },
}

struct Provider {
    store: PathBuf,
    prj: &'static Prj,
    sessions: Mutex<HashMap<[u8; 16], Session>>,
    notifications: Mutex<Vec<Notification>>,
}

impl Provider {
    /// 枚举快照:store 下 rel_dir 的直接子项,按 PrjFileNameCompare 序
    /// (ProjFS 要求填充顺序与该比较器一致)。
    fn snapshot(&self, rel_dir: &str) -> Vec<Entry> {
        let dir = self.store.join(rel_dir);
        let mut entries: Vec<(Entry, Vec<u16>)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            entries.push((
                Entry {
                    name,
                    is_dir: md.is_dir(),
                    size: md.len() as i64,
                    write_time: filetime_from(md.modified().ok().unwrap_or(UNIX_EPOCH)),
                },
                wide,
            ));
        }
        unsafe {
            entries.sort_by(|a, b| {
                (self.prj.file_name_compare)(PCWSTR(a.1.as_ptr()), PCWSTR(b.1.as_ptr())).cmp(&0)
            });
        }
        entries.into_iter().map(|(e, _)| e).collect()
    }

    fn backing(&self, rel: &str) -> PathBuf {
        self.store.join(rel)
    }
}

fn provider_of(cbdata: &PRJ_CALLBACK_DATA) -> &'static Provider {
    unsafe { &*(cbdata.InstanceContext as *const Provider) }
}

/// 回调路径 → 相对路径("\sub\f" → "sub\f";"\" → "")
fn rel_of(p: PCWSTR) -> String {
    unsafe {
        let mut end = 0usize;
        while *p.0.add(end) != 0 {
            end += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, end));
        s.trim_matches('\\').to_string()
    }
}

fn guid_key(g: &GUID) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&g.data1.to_ne_bytes());
    k[4..6].copy_from_slice(&g.data2.to_ne_bytes());
    k[6..8].copy_from_slice(&g.data3.to_ne_bytes());
    k[8..16].copy_from_slice(&g.data4);
    k
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn filetime_from(t: std::time::SystemTime) -> i64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() * 10_000_000 + d.subsec_nanos() as u64 / 100) as i64 + 11_644_473_600_000_000
}

fn ft_parts(t: i64) -> FILETIME {
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: ((t as u64) >> 32) as u32,
    }
}

unsafe extern "system" fn cb_start_enum(
    cbdata: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        let rel_dir = rel_of((*cbdata).FilePathName);
        let entries = p.snapshot(&rel_dir);
        p.sessions
            .lock()
            .unwrap()
            .insert(guid_key(&*enumeration_id), Session { entries, cursor: 0 });
        S_OK
    }
}

unsafe extern "system" fn cb_end_enum(
    cbdata: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        p.sessions.lock().unwrap().remove(&guid_key(&*enumeration_id));
        S_OK
    }
}

unsafe extern "system" fn cb_get_enum(
    cbdata: *const PRJ_CALLBACK_DATA,
    enumeration_id: *const GUID,
    search_expression: PCWSTR,
    buffer: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        let restart = (*cbdata).Flags == PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN;
        let mut sessions = p.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&guid_key(&*enumeration_id)) else {
            return E_INVALID_ARG;
        };
        if restart {
            session.cursor = 0;
        }
        while session.cursor < session.entries.len() {
            let e = &session.entries[session.cursor];
            // 必须按 searchExpression 过滤(字面名查询若全量返回,调用方会把
            // 整个目录当匹配集;ADR-0003 工程事实 4)
            let name_w: Vec<u16> = e.name.encode_utf16().chain([0]).collect();
            if (p.prj.file_name_match)(PCWSTR(name_w.as_ptr()), search_expression) == BOOLEAN(0) {
                session.cursor += 1;
                continue;
            }
            let ft = ft_parts(e.write_time);
            let info = PRJ_FILE_BASIC_INFO {
                IsDirectory: BOOLEAN(if e.is_dir { 1 } else { 0 }),
                FileSize: e.size,
                CreationTime: ft.dwLowDateTime as i64,
                LastAccessTime: ft.dwLowDateTime as i64,
                LastWriteTime: ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64,
                ChangeTime: 0,
                FileAttributes: 0,
            };
            let hr = (p.prj.fill_dir_entry)(PCWSTR(name_w.as_ptr()), &info as *const _, buffer);
            if hr == E_INSUFFICIENT_BUFFER {
                return S_OK; // 缓冲满:游标停在当前条目,ProjFS 排空后再次回调
            }
            if hr.0 < 0 {
                return hr;
            }
            session.cursor += 1;
        }
        S_OK
    }
}

unsafe extern "system" fn cb_get_placeholder(cbdata: *const PRJ_CALLBACK_DATA) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        let rel = rel_of((*cbdata).FilePathName);
        let Ok(md) = std::fs::metadata(p.backing(&rel)) else {
            return E_FILE_NOT_FOUND;
        };
        let mtime = filetime_from(md.modified().unwrap_or(UNIX_EPOCH));
        let ft = ft_parts(mtime);
        let mut info: PRJ_PLACEHOLDER_INFO = std::mem::zeroed();
        info.FileBasicInfo = PRJ_FILE_BASIC_INFO {
            IsDirectory: BOOLEAN(if md.is_dir() { 1 } else { 0 }),
            FileSize: md.len() as i64,
            CreationTime: ft.dwLowDateTime as i64,
            LastAccessTime: 0,
            LastWriteTime: ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64,
            ChangeTime: 0,
            FileAttributes: 0,
        };
        // ContentID = provider 定义的文件身份(size+mtime);ProjFS 据此判定
        // 占位内容是否需要重新投影
        info.VersionInfo.ProviderID[0..4].copy_from_slice(b"sbx!");
        info.VersionInfo.ContentID[0..8].copy_from_slice(&md.len().to_be_bytes());
        info.VersionInfo.ContentID[8..16].copy_from_slice(&mtime.to_be_bytes());
        (p.prj.write_placeholder)(
            (*cbdata).NamespaceVirtualizationContext,
            (*cbdata).FilePathName,
            &info,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    }
}

unsafe extern "system" fn cb_get_file_data(
    cbdata: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        let rel = rel_of((*cbdata).FilePathName);
        let Ok(mut f) = std::fs::File::open(p.backing(&rel)) else {
            return E_FILE_NOT_FOUND;
        };
        if f.seek(SeekFrom::Start(byte_offset)).is_err() {
            return E_FILE_NOT_FOUND;
        }
        let mut buf = vec![0u8; length as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return E_FILE_NOT_FOUND,
            }
        }
        (p.prj.write_file_data)(
            (*cbdata).NamespaceVirtualizationContext,
            &(*cbdata).DataStreamId,
            buf.as_ptr() as *const core::ffi::c_void,
            byte_offset,
            length,
        )
    }
}

unsafe extern "system" fn cb_notification(
    cbdata: *const PRJ_CALLBACK_DATA,
    _is_directory: BOOLEAN,
    notification: PRJ_NOTIFICATION,
    destination_filename: PCWSTR,
    _operation_parameters: *mut windows::Win32::Storage::ProjectedFileSystem::PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    unsafe {
        let p = provider_of(&*cbdata);
        let Some(fp) = (if (*cbdata).FilePathName.0.is_null() {
            None
        } else {
            Some((*cbdata).FilePathName)
        }) else {
            return S_OK;
        };
        let path = rel_of(fp);
        // destination_filename 按通知种类可空,只在需要时解引用
        let dest = || {
            if destination_filename.0.is_null() {
                String::new()
            } else {
                rel_of(destination_filename)
            }
        };
        let n = match notification {
            PRJ_NOTIFICATION_PRE_DELETE => Some(Notification::PreDelete { path }),
            PRJ_NOTIFICATION_PRE_RENAME => Some(Notification::PreRename { path, dest: dest() }),
            PRJ_NOTIFICATION_FILE_RENAMED => Some(Notification::Renamed { path, dest: dest() }),
            PRJ_NOTIFICATION_FILE_OVERWRITTEN => Some(Notification::Overwritten { path }),
            PRJ_NOTIFICATION_NEW_FILE_CREATED => Some(Notification::NewFile { path }),
            _ => None,
        };
        if let Some(n) = n {
            p.notifications.lock().unwrap().push(n);
        }
        // v1 恒放行;pre-delete/pre-rename 的策略 veto 是治理接线事项(M3)
        S_OK
    }
}

// ---------------------------------------------------------------- View

/// ProjFS 视图错误。
#[derive(Debug)]
pub enum ProjFsError {
    /// Client-ProjFS 可选功能未启用(ProjectedFSLib.dll 缺失)
    NotAvailable,
    /// 调用失败(hr = HRESULT 形态)
    Hr { hr: i32, context: String },
    /// 本地 IO 错误
    Io(std::io::Error),
}

impl std::fmt::Display for ProjFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjFsError::NotAvailable => write!(
                f,
                "ProjFS 不可用:一次性启用(需 admin)DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
            ),
            ProjFsError::Hr { hr, context } => {
                write!(f, "ProjFS 调用失败 {context}: 0x{:08X}", *hr as u32)
            }
            ProjFsError::Io(e) => write!(f, "ProjFS IO 错误: {e}"),
        }
    }
}

impl From<std::io::Error> for ProjFsError {
    fn from(e: std::io::Error) -> Self {
        ProjFsError::Io(e)
    }
}

/// 视图相对路径上的一条变更。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// 视图新建(store 无此文件)
    New(String),
    /// 视图改写(store 同名文件内容不同)
    Modified(String),
    /// 视图删除(store 有此文件)
    Deleted(String),
}

/// merge/discard 的结果报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    pub applied: usize,
    pub deleted: usize,
    /// 因未确认而跳过的 Deleted 项数(merge_with 独有;保守跳过,不报错)
    pub skipped_deletions: usize,
}

/// merge 的删除策略(ADR-0004:文件级 DACL 不可靠拦截 DELETE,
/// 真实删除的唯一闸门在 merge)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergePolicy {
    /// 默认:Deleted 项跳过真实删除(视图还原为占位),计数上报——
    /// "删除需要显式确认"的保守语义
    #[default]
    ConfirmDeletions,
    /// 调用方已确认:Deleted 项执行真实删除
    AllowDeletions,
}

/// 一个ProjFS 虚拟视图实例(root = 合并视图,store = 只读基底)。
pub struct View {
    prj: &'static Prj,
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    root: PathBuf,
    store: PathBuf,
    _provider: *mut Provider,
}

unsafe impl Send for View {}

impl View {
    /// 启动视图:root 必须为空目录(不存在则创建;非空报错——ProjFS 要求)。
    pub fn start(store: &Path, root: &Path) -> Result<View, ProjFsError> {
        let Some(prj) = Prj::get() else {
            return Err(ProjFsError::NotAvailable);
        };
        std::fs::create_dir_all(store)?;
        std::fs::create_dir_all(root)?;
        if std::fs::read_dir(root)?.next().is_some() {
            return Err(ProjFsError::Hr {
                hr: E_INVALID_ARG.0,
                context: "视图根目录必须为空".into(),
            });
        }
        let provider = Box::into_raw(Box::new(Provider {
            store: store.to_path_buf(),
            prj,
            sessions: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
        }));
        let start = |provider: *mut Provider| -> Result<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, (i32, String)> {
            let cbs = PRJ_CALLBACKS {
                StartDirectoryEnumerationCallback: Some(cb_start_enum),
                EndDirectoryEnumerationCallback: Some(cb_end_enum),
                GetDirectoryEnumerationCallback: Some(cb_get_enum),
                GetPlaceholderInfoCallback: Some(cb_get_placeholder),
                GetFileDataCallback: Some(cb_get_file_data),
                QueryFileNameCallback: None,
                NotificationCallback: Some(cb_notification),
                CancelCommandCallback: None,
            };
            let root_w: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
            // 通知映射:NotificationRoot = 相对根的路径,**空串 = 整个根**
            // ("\\" 是非法形态,StartVirtualizing 报 E_INVALIDARG——实测)
            let notif_root: Vec<u16> = vec![0];
            let mut mappings = [PRJ_NOTIFICATION_MAPPING {
                NotificationBitMask: PRJ_NOTIFY_TYPES(
                    PRJ_NOTIFY_PRE_DELETE.0
                        | PRJ_NOTIFY_PRE_RENAME.0
                        | PRJ_NOTIFY_FILE_RENAMED.0
                        | PRJ_NOTIFY_FILE_OVERWRITTEN.0
                        | PRJ_NOTIFY_NEW_FILE_CREATED.0,
                ),
                NotificationRoot: PCWSTR(notif_root.as_ptr()),
            }];
            let options = PRJ_STARTVIRTUALIZING_OPTIONS {
                NotificationMappings: mappings.as_mut_ptr(),
                NotificationMappingsCount: 1,
                ..Default::default()
            };
            // 两步流程:先打 reparse 占位标记(漏一步报 0x80071126),再启动
            let instance_id = GUID::from_u128(
                (std::process::id() as u128) << 96
                    | (SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                        & 0xFFFF_FFFF_FFFF_FFFF),
            );
            let hr = unsafe {
                (prj.mark_dir_as_placeholder)(
                    PCWSTR(root_w.as_ptr()),
                    PCWSTR::null(),
                    std::ptr::null(),
                    &instance_id,
                )
            };
            if hr.0 < 0 {
                return Err((hr.0, "MarkDirectoryAsPlaceholder".into()));
            }
            let mut ctx = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();
            let hr = unsafe {
                (prj.start_virtualizing)(
                    PCWSTR(root_w.as_ptr()),
                    &cbs,
                    provider as *const core::ffi::c_void,
                    &options,
                    &mut ctx,
                )
            };
            if hr.0 < 0 {
                return Err((hr.0, "StartVirtualizing".into()));
            }
            Ok(ctx)
        };
        match start(provider) {
            Ok(ctx) => Ok(View {
                prj,
                ctx,
                root: root.to_path_buf(),
                store: store.to_path_buf(),
                _provider: provider,
            }),
            Err((hr, context)) => {
                drop(unsafe { Box::from_raw(provider) });
                Err(ProjFsError::Hr { hr, context })
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &Path {
        &self.store
    }

    /// 视图相对路径 → 视图内绝对路径
    fn view_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// 相对路径 → 完整路径形态(状态 API 实测只认完整路径)
    fn full(&self, rel: &str) -> String {
        self.view_path(rel).to_string_lossy().into_owned()
    }

    /// PrjDeleteFile 路径形态(实证):完整路径报 ERROR_INVALID_NAME(0x7B),
    /// 只认**纯相对路径**(不带根反斜杠)——与 GetOnDiskFileState(只认完整
    /// 路径)相反,ProjFS 路径形态约定按 API 各自适配。
    fn delete_view_file(&self, rel: &str) -> Result<(), i32> {
        let plain = wide(rel);
        let hr = unsafe {
            (self.prj.delete_file)(
                self.ctx,
                PCWSTR(plain.as_ptr()),
                UPDATE_ALLOW_ALL,
                std::ptr::null_mut(),
            )
        };
        if hr.0 >= 0 {
            Ok(())
        } else {
            Err(hr.0)
        }
    }

    fn on_disk_state(&self, rel: &str) -> Option<PRJ_FILE_STATE> {
        let full = wide(&self.full(rel));
        let mut state = PRJ_FILE_STATE(0);
        let hr = unsafe {
            (self.prj.get_on_disk_file_state)(PCWSTR(full.as_ptr()), &mut state)
        };
        if hr.0 < 0 {
            None
        } else {
            Some(state)
        }
    }

    /// 递归收集 `base`(视图或 store)下的相对文件路径。
    fn walk_files(dir: &Path, rel_prefix: &str, out: &mut Vec<String>) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{rel_prefix}\\{name}")
            };
            if e.file_type()?.is_dir() {
                Self::walk_files(&e.path(), &rel, out)?;
            } else {
                out.push(rel);
            }
        }
        Ok(())
    }

    /// 触发视图全树枚举,把 store 目录物化为占位目录。只枚举不读内容,
    /// 不触发文件水合——视图侧挂 ACE 前要求对象已存在(占位或实体)。
    pub fn materialize_tree(&self) -> std::io::Result<()> {
        let mut out = Vec::new();
        Self::walk_files(&self.root, "", &mut out)
    }

    /// 视图文件与 store 同名文件内容是否不同(先比 size 再逐字节)。
    /// diff 去噪:占位被元数据/DACL 操作弄脏但内容未变不算 Modified。
    fn contents_differ(view_file: &Path, store_file: &Path) -> bool {
        let (Ok(mv), Ok(ms)) = (std::fs::metadata(view_file), std::fs::metadata(store_file)) else {
            return true;
        };
        if mv.len() != ms.len() {
            return true;
        }
        match (std::fs::read(view_file), std::fs::read(store_file)) {
            (Ok(a), Ok(b)) => a != b,
            // 读失败保守视为不同(turn 边界宁多报不漏报)
            _ => true,
        }
    }

    /// store 侧路径是否为 reparse point(symlink/junction)。
    /// merge 落盘会写穿 symlink 目标、删除语义也退化为"删链接"——一律
    /// fail-closed(沙箱边界不得被 store 内预埋的链接越过)。
    fn store_is_reparse(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.store.join(rel))
            .map(|md| md.file_attributes() & crate::acl::FILE_ATTRIBUTE_REPARSE_POINT != 0)
            .unwrap_or(false)
    }

    /// turn 边界差异:视图相对 store 的变更清单。
    ///
    /// 判定(全部不触发水合;Modified 候选才读内容比对):
    /// - 视图枚举 = 存活命名空间(含未投影的 store 文件,排除 tombstone);
    /// - store 侧文件不在存活列表 = 视图内删除;
    /// - 视图内磁盘状态为 FULL/脏占位 = 改写(store 有同名,且内容确有
    ///   差异)/新建(无同名)。
    pub fn diff(&self) -> std::io::Result<Vec<Change>> {
        let mut changes = Vec::new();
        let mut live = Vec::new();
        Self::walk_files(&self.root, "", &mut live)?;
        // 大小写折叠比对:ProjFS 枚举可能返回与 store 不同的形态(如 store
        // "README.md" / 投影枚举 "readme.md"),NTFS 大小写不敏感——按原样
        // 比对会把同一文件判成 Modified + Deleted 双变更(merge 时先 copy
        // 后 PrjDeleteFile 又把刚落的实体当占位删掉,实证过的坑)
        let live_lower: std::collections::HashSet<String> =
            live.iter().map(|r| r.to_lowercase()).collect();

        let mut store_files = Vec::new();
        Self::walk_files(&self.store, "", &mut store_files)?;
        for rel in &store_files {
            if !live_lower.contains(&rel.to_lowercase()) {
                changes.push(Change::Deleted(rel.clone()));
            }
        }

        for rel in &live {
            let state = self.on_disk_state(rel);
            let dirty = match state {
                Some(s) => s.0 & (STATE_FULL | STATE_DIRTY_PLACEHOLDER) != 0,
                None => true, // 磁盘实体却查无占位状态 = 原生 upper 文件
            };
            if dirty && self.store.join(&rel).is_file() {
                if Self::contents_differ(&self.view_path(&rel), &self.store.join(&rel)) {
                    changes.push(Change::Modified(rel.clone()));
                }
            } else if dirty {
                changes.push(Change::New(rel.clone()));
            }
        }
        changes.sort();
        Ok(changes)
    }

    /// turn 边界合并:把视图变更落回 store,并把视图侧还原为占位
    /// (后续访问按合并后的 store 重新投影)→ 合并后 diff 归零。
    pub fn merge(&self) -> std::io::Result<Report> {
        self.merge_with(MergePolicy::ConfirmDeletions)
    }

    /// [`View::merge`] 的策略形态(ADR-0004)。
    ///
    /// 删除语义边界:文件级 DACL 无法可靠拦截 DELETE(父目录
    /// FILE_DELETE_CHILD 路径不可控),因此真实删除的唯一闸门在 merge。
    /// - [`MergePolicy::ConfirmDeletions`](默认):Deleted 项**不落盘**,跳过
    ///   并计数(`report.skipped_deletions`);视图侧还原为占位(文件重新
    ///   投影,内容与 store 一致)——删除不生效,但也不再悬在名单里;
    /// - [`MergePolicy::AllowDeletions`]:Deleted 项执行真实删除(调用方已
    ///   逐条确认)。
    pub fn merge_with(&self, policy: MergePolicy) -> std::io::Result<Report> {
        let mut report = Report::default();
        for change in self.diff()? {
            let rel = match change {
                Change::New(rel) | Change::Modified(rel) => rel,
                Change::Deleted(rel) => {
                    if self.store_is_reparse(&rel) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!(
                                "merge 拒绝:store 侧 {rel} 是 reparse point(链接不可代删,请人工处置)"
                            ),
                        ));
                    }
                    if policy == MergePolicy::ConfirmDeletions {
                        // 跳过真实删除;视图侧撤销 tombstone(文件重新投影)
                        report.skipped_deletions += 1;
                        if self.delete_view_file(&rel).is_ok() {
                            report.applied += 1;
                        }
                        continue;
                    }
                    let dst = self.store.join(&rel);
                    if dst.is_file() {
                        std::fs::remove_file(dst)?;
                    }
                    // 撤销 tombstone;store 已无此文件,重新投影自然缺席
                    if self.delete_view_file(&rel).is_ok() {
                        report.deleted += 1;
                    }
                    continue;
                }
            };
            if self.store_is_reparse(&rel) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "merge 拒绝:store 侧 {rel} 是 reparse point(写入会穿透链接越过沙箱边界)"
                    ),
                ));
            }
            let dst = self.store.join(&rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(self.view_path(&rel), dst)?;
            // 视图侧还原为占位(下次访问按新 store 重新投影)
            if self.delete_view_file(&rel).is_ok() {
                report.applied += 1;
            }
        }
        Ok(report)
    }

    /// turn 边界丢弃:毫秒级撤销全部视图变更。视图保持可用:
    /// 新建文件被删除,改写文件还原为占位(下次访问按 store 重新投影),
    /// 删除的 tombstone 被撤销(重新投影)。
    pub fn discard(&self) -> std::io::Result<Report> {
        let mut report = Report::default();
        for change in self.diff()? {
            let ok = match &change {
                Change::New(rel) => std::fs::remove_file(self.view_path(rel)).is_ok(),
                Change::Modified(rel) | Change::Deleted(rel) => {
                    self.delete_view_file(rel).is_ok()
                }
            };
            if ok {
                match change {
                    Change::Deleted(_) => report.deleted += 1,
                    _ => report.applied += 1,
                }
            }
        }
        Ok(report)
    }
    /// 取走累计的视图通知(观察流;View 存活期间任意时刻可取,Drop 后无效)。
    pub fn take_notifications(&self) -> Vec<Notification> {
        let provider = unsafe { &*self._provider };
        std::mem::take(&mut *provider.notifications.lock().unwrap())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe {
            (self.prj.stop_virtualizing)(self.ctx);
            drop(Box::from_raw(self._provider));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_roundtrip_shape() {
        let t = filetime_from(UNIX_EPOCH);
        // FILETIME 纪元偏移:1970-01-01 = 116444736000000000(×100ns)
        assert_eq!(t, 11_644_473_600_000_000);
    }

    /// 诊断:PrjGetOnDiskFileState 的路径形态(完整路径 / 根相对)
    #[test]
    fn diag_state_path_forms() {
        let Some(prj) = Prj::get() else {
            eprintln!("skip: ProjFS 不可用");
            return;
        };
        let base = std::env::temp_dir().join(format!("sbx-projfs-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store = base.join("store");
        let root = base.join("view");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(store.join("doc.txt"), "original").unwrap();
        let provider = Box::into_raw(Box::new(Provider {
            store: store.clone(),
            prj,
            sessions: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
        }));
        let cbs = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(cb_start_enum),
            EndDirectoryEnumerationCallback: Some(cb_end_enum),
            GetDirectoryEnumerationCallback: Some(cb_get_enum),
            GetPlaceholderInfoCallback: Some(cb_get_placeholder),
            GetFileDataCallback: Some(cb_get_file_data),
            QueryFileNameCallback: None,
            NotificationCallback: None,
            CancelCommandCallback: None,
        };
        let root_w: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
        let instance_id = GUID::from_u128(42);
        let mut ctx = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();
        unsafe {
            let hr = (prj.mark_dir_as_placeholder)(
                PCWSTR(root_w.as_ptr()),
                PCWSTR::null(),
                std::ptr::null(),
                &instance_id,
            );
            assert!(hr.0 >= 0, "mark hr=0x{:08X}", hr.0 as u32);
            let hr = (prj.start_virtualizing)(
                PCWSTR(root_w.as_ptr()),
                &cbs,
                provider as *const core::ffi::c_void,
                std::ptr::null(),
                &mut ctx,
            );
            assert!(hr.0 >= 0, "start hr=0x{:08X}", hr.0 as u32);
        }
        std::fs::write(root.join("doc.txt"), "modified").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        // 形态 A:完整路径;形态 B:根相对(带反斜杠);形态 C:根相对(不带)
        for (tag, path) in [
            ("full", root.join("doc.txt").to_string_lossy().into_owned()),
            ("rel-backslash", "\\doc.txt".to_string()),
            ("rel-plain", "doc.txt".to_string()),
        ] {
            let w = wide(&path);
            let mut state = PRJ_FILE_STATE(0);
            let hr = unsafe { (prj.get_on_disk_file_state)(PCWSTR(w.as_ptr()), &mut state) };
            eprintln!("diag[{tag}] hr=0x{:08X} state={}", hr.0 as u32, state.0);
        }
        unsafe {
            (prj.stop_virtualizing)(ctx);
            drop(Box::from_raw(provider));
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
