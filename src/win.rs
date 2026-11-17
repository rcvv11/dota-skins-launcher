//! Thin Win32 bindings: process list, top-level windows, sounds, registry.
//! Only public, documented user-mode APIs; nothing touches another process's memory.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HWND = isize;

#[repr(C)]
struct PROCESSENTRY32W {
    dwSize: u32,
    cntUsage: u32,
    th32ProcessID: u32,
    th32DefaultHeapID: usize,
    th32ModuleID: u32,
    cntThreads: u32,
    th32ParentProcessID: u32,
    pcPriClassBase: i32,
    dwFlags: u32,
    szExeFile: [u16; 260],
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Process32FirstW(snap: isize, entry: *mut PROCESSENTRY32W) -> i32;
    fn Process32NextW(snap: isize, entry: *mut PROCESSENTRY32W) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn TerminateProcess(h: isize, code: u32) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
    fn GetTokenInformation(token: isize, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> isize;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(hwnd: HWND, op: *const u16, file: *const u16, params: *const u16, dir: *const u16, show: i32) -> isize;
}

/// Whether this process runs with administrator rights (elevated token).
pub fn is_elevated() -> bool {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: u32 = 20;
    unsafe {
        let mut token = 0isize;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevated = 0u32;
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TOKEN_ELEVATION, &mut elevated as *mut u32 as *mut c_void, 4, &mut len);
        CloseHandle(token);
        ok != 0 && elevated != 0
    }
}

/// Starts this exe again with the UAC prompt ("runas"). False when the user
/// declined or it could not start.
pub fn relaunch_elevated() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let args: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
        .collect();
    let (op, file, params) = (wide("runas"), wide(&exe.to_string_lossy()), wide(&args.join(" ")));
    let dir = exe.parent().map(|d| wide(&d.to_string_lossy())).unwrap_or_else(|| wide(""));
    // SW_SHOWNORMAL; > 32 means started
    unsafe { ShellExecuteW(0, op.as_ptr(), file.as_ptr(), params.as_ptr(), dir.as_ptr(), 1) > 32 }
}

/// Ends the process that owns `hwnd` (never this process).
pub fn terminate_window_process(hwnd: HWND) {
    const PROCESS_TERMINATE: u32 = 0x0001;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 || pid == std::process::id() {
        return;
    }
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h != 0 {
            TerminateProcess(h, 0);
            CloseHandle(h);
        }
    }
}

type EnumProc = extern "system" fn(HWND, isize) -> i32;

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: EnumProc, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: HWND) -> i32;
    fn PostMessageW(hwnd: HWND, msg: u32, w: usize, l: isize) -> i32;
    fn FindWindowW(class: *const u16, title: *const u16) -> HWND;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(hkey: isize, subkey: *const u16, value: *const u16, kind: u32, data: *const c_void, size: u32) -> i32;
    fn RegDeleteKeyValueW(hkey: isize, subkey: *const u16, value: *const u16) -> i32;
    fn RegGetValueW(
        hkey: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
}

const TH32CS_SNAPPROCESS: u32 = 0x2;
const WM_CLOSE: u32 = 0x0010;
const RRF_RT_REG_SZ: u32 = 0x2;
// HKEY_CURRENT_USER is ((HKEY)(ULONG_PTR)((LONG)0x80000001)): sign-extended on x64.
const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// PIDs of running processes whose executable name matches (case-insensitive).
pub fn find_processes(exe_name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == -1 {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case(exe_name) {
                out.push(entry.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    out
}

struct WindowSearch {
    pid: u32,
    found: Vec<HWND>,
}

extern "system" fn collect_windows(hwnd: HWND, lparam: isize) -> i32 {
    unsafe {
        let search = &mut *(lparam as *mut WindowSearch);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == search.pid && IsWindowVisible(hwnd) != 0 {
            search.found.push(hwnd);
        }
    }
    1
}

/// Asks every visible top-level window of `pid` to close (same as clicking ✕).
/// Returns how many windows received WM_CLOSE.
pub fn request_close(pid: u32) -> usize {
    let mut search = WindowSearch {
        pid,
        found: Vec::new(),
    };
    unsafe {
        EnumWindows(collect_windows, &mut search as *mut WindowSearch as isize);
        for &hwnd in &search.found {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
    search.found.len()
}

pub fn find_window(title: &str) -> Option<HWND> {
    let t = wide(title);
    let h = unsafe { FindWindowW(std::ptr::null(), t.as_ptr()) };
    (h != 0).then_some(h)
}

/// Writes (Some) or deletes (None) a REG_SZ value under HKEY_CURRENT_USER.
pub fn write_hkcu_string(subkey: &str, value: &str, data: Option<&str>) -> Result<(), String> {
    let sk = wide(subkey);
    let v = wide(value);
    let status = unsafe {
        match data {
            Some(text) => {
                let d = wide(text);
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    sk.as_ptr(),
                    v.as_ptr(),
                    1, // REG_SZ
                    d.as_ptr() as *const c_void,
                    (d.len() * 2) as u32,
                )
            }
            None => RegDeleteKeyValueW(HKEY_CURRENT_USER, sk.as_ptr(), v.as_ptr()),
        }
    };
    // 2 = ERROR_FILE_NOT_FOUND: deleting a value that is already gone is fine
    if status == 0 || (data.is_none() && status == 2) {
        Ok(())
    } else {
        Err(crate::trf!("реестр: ошибка {status}", "registry: error {status}"))
    }
}

/// REG_SZ value from HKEY_CURRENT_USER.
pub fn read_hkcu_string(subkey: &str, value: &str) -> Option<String> {
    let sk = wide(subkey);
    let v = wide(value);
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sk.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}
