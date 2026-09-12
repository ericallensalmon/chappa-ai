//! Crash evidence: a minidump plus a plain-text report for every native
//! crash, a module inventory at every launch, and a panic report.
//!
//! Why this exists: DLLs that other software injects into the process
//! (antivirus behavioral hooks, overlays, input methods, accessibility
//! tools) run with our privileges in our address space, and a bug in one
//! of them corrupts our heap exactly like a bug of ours would. The crash
//! then lands in whatever code touches the corrupted memory next, which
//! is unrelated to the culprit. The one fact that resolves such a report
//! is the list of foreign modules loaded in the process, so that list is
//! written at startup (`modules.txt`) as well as into every crash report.
//! The startup copy matters because heap-corruption fast-fails and some
//! callback exceptions go straight to the OS error reporter and never
//! reach an in-process handler.
//!
//! Files land in the app log directory:
//!
//! - `modules.txt` — rewritten at every launch.
//! - `crash-<unix-seconds>-<pid>.dmp` + `.txt` — native crash.
//! - `panic-<unix-seconds>-<pid>.txt` — Rust panic.
//!
//! The handler writes the dump in-process. That is less robust than an
//! out-of-process watcher, but it needs no helper executable, and the
//! app's own allocations do not live in the OS heap (see the global
//! allocator in lib.rs), so a corrupted OS heap does not take the handler
//! down with it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where reports go. Set once by [`install`]; read by the crash filter,
/// which must not allocate more than it has to.
static REPORT_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Who a loaded module belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Under the Windows directory: the OS itself.
    System,
    /// The app's own executable or something beside it.
    App,
    /// Anything else — injected by other software, or a runtime the app
    /// loads from elsewhere (the web view engine, for one).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub path: String,
    pub base: usize,
    pub size: usize,
    pub origin: Origin,
}

impl Module {
    fn contains(&self, address: usize) -> bool {
        address >= self.base && address < self.base.saturating_add(self.size)
    }
}

/// Classify a module path against the OS and app directories. Pure so it
/// can be tested without loading anything; the comparison is
/// case-insensitive because Windows paths are.
pub fn classify(path: &str, windows_dir: &str, app_dir: &str) -> Origin {
    let lower = path.to_ascii_lowercase();
    if starts_with_dir(&lower, &windows_dir.to_ascii_lowercase()) {
        Origin::System
    } else if starts_with_dir(&lower, &app_dir.to_ascii_lowercase()) {
        Origin::App
    } else {
        Origin::Other
    }
}

fn starts_with_dir(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return false;
    }
    let dir = dir.trim_end_matches(['\\', '/']);
    path.starts_with(dir) && matches!(path.as_bytes().get(dir.len()), Some(b'\\') | Some(b'/'))
}

/// The plain-text report for a native crash: what faulted, where, and
/// every loaded module with the foreign ones called out first.
pub fn render_crash_report(code: u32, address: usize, modules: &[Module]) -> String {
    let mut out = String::new();
    out.push_str(&format!("exception 0x{code:08x} at 0x{address:016x}\n"));
    match modules.iter().find(|m| m.contains(address)) {
        Some(m) => out.push_str(&format!(
            "in {}+0x{:x}\n",
            file_name(&m.path),
            address - m.base
        )),
        None => out.push_str("in <no loaded module>\n"),
    }
    out.push('\n');
    out.push_str(&render_module_inventory(modules));
    out
}

/// The module inventory: foreign modules first, then the rest.
pub fn render_module_inventory(modules: &[Module]) -> String {
    let mut out = String::new();
    let foreign: Vec<&Module> = modules
        .iter()
        .filter(|m| m.origin == Origin::Other)
        .collect();
    out.push_str(&format!("foreign modules: {}\n", foreign.len()));
    for m in &foreign {
        out.push_str(&format!("  {}\n", m.path));
    }
    out.push_str(&format!("\nall modules: {}\n", modules.len()));
    for m in modules {
        out.push_str(&format!(
            "  0x{:016x} {:>9} {:<6} {}\n",
            m.base,
            m.size,
            match m.origin {
                Origin::System => "system",
                Origin::App => "app",
                Origin::Other => "other",
            },
            m.path
        ));
    }
    out
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}-{}", std::process::id())
}

/// Install everything: the module inventory file, the panic report hook,
/// and (Windows) the native crash filter. Safe to call once per process;
/// a second call keeps the first directory.
pub fn install(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let dir = REPORT_DIR.get_or_init(|| dir).clone();

    let modules = loaded_modules();
    let inventory = render_module_inventory(&modules);
    let _ = std::fs::write(dir.join("modules.txt"), &inventory);
    let foreign = modules.iter().filter(|m| m.origin == Origin::Other).count();
    log::info!(
        "crash reporting to {}; {foreign} foreign module(s) loaded",
        dir.display()
    );

    install_panic_hook(dir.clone());
    platform::install_native_filter();
}

fn install_panic_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut text = format!("panic: {info}\n\n");
        text.push_str(&render_module_inventory(&loaded_modules()));
        let _ = std::fs::write(dir.join(format!("panic-{}.txt", stamp())), text);
        previous(info);
    }));
}

/// Every module mapped into this process, classified. Empty off Windows.
pub fn loaded_modules() -> Vec<Module> {
    platform::loaded_modules()
}

/// The report directory, once installed.
pub fn report_dir() -> Option<&'static Path> {
    REPORT_DIR.get().map(PathBuf::as_path)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, HMODULE};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MiniDumpWithDataSegs, MiniDumpWithHandleData, MiniDumpWithIndirectlyReferencedMemory,
        MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
        SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
        MINIDUMP_EXCEPTION_INFORMATION,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32EnumProcessModules, K32GetModuleFileNameExW, K32GetModuleInformation, MODULEINFO,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
    };

    pub fn install_native_filter() {
        // SAFETY: registering a plain function pointer with the OS.
        unsafe {
            SetUnhandledExceptionFilter(Some(filter));
        }
    }

    /// Runs on the crashing thread after every other handler declined.
    /// Writes the dump and the report, then lets the OS carry on with its
    /// own reporting (`EXCEPTION_CONTINUE_SEARCH`), so nothing here hides
    /// the crash from the system error reporter.
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        if let Some(dir) = REPORT_DIR.get() {
            let stamp = stamp();
            let (code, address) = exception_summary(info);
            write_dump(&dir.join(format!("crash-{stamp}.dmp")), info);
            let report = render_crash_report(code, address, &loaded_modules());
            let _ = std::fs::write(dir.join(format!("crash-{stamp}.txt")), report);
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe fn exception_summary(info: *const EXCEPTION_POINTERS) -> (u32, usize) {
        if info.is_null() {
            return (0, 0);
        }
        let record = (*info).ExceptionRecord;
        if record.is_null() {
            return (0, 0);
        }
        (
            (*record).ExceptionCode as u32,
            (*record).ExceptionAddress as usize,
        )
    }

    unsafe fn write_dump(path: &Path, info: *const EXCEPTION_POINTERS) {
        let Ok(file) = std::fs::File::create(path) else {
            return;
        };
        let exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: GetCurrentThreadId(),
            ExceptionPointers: info as *mut EXCEPTION_POINTERS,
            ClientPointers: 0,
        };
        // Enough for a symbolized stack on every thread plus the memory
        // those stacks point at; a full-memory dump would be hundreds of
        // megabytes and mostly web view.
        let kind = MiniDumpWithDataSegs
            | MiniDumpWithHandleData
            | MiniDumpWithIndirectlyReferencedMemory
            | MiniDumpWithThreadInfo
            | MiniDumpWithUnloadedModules;
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            file.as_raw_handle() as HANDLE,
            kind,
            if info.is_null() {
                std::ptr::null()
            } else {
                &exception
            },
            std::ptr::null(),
            std::ptr::null(),
        );
    }

    pub fn loaded_modules() -> Vec<Module> {
        let windows_dir = std::env::var("SystemRoot").unwrap_or_default();
        let app_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default();

        let mut handles: Vec<HMODULE> = vec![std::ptr::null_mut(); 512];
        let mut needed: u32 = 0;
        // SAFETY: the buffer and its byte size are passed together; the
        // API writes at most `cb` bytes and reports the full size needed.
        let ok = unsafe {
            K32EnumProcessModules(
                GetCurrentProcess(),
                handles.as_mut_ptr(),
                (handles.len() * std::mem::size_of::<HMODULE>()) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        let count = (needed as usize / std::mem::size_of::<HMODULE>()).min(handles.len());
        handles.truncate(count);

        handles
            .into_iter()
            .map(|h| {
                let mut name = [0u16; 1024];
                // SAFETY: fixed-size buffer with its length in characters.
                let len = unsafe {
                    K32GetModuleFileNameExW(
                        GetCurrentProcess(),
                        h,
                        name.as_mut_ptr(),
                        name.len() as u32,
                    )
                } as usize;
                let path = String::from_utf16_lossy(&name[..len.min(name.len())]);
                let mut mi = MODULEINFO {
                    lpBaseOfDll: std::ptr::null_mut(),
                    SizeOfImage: 0,
                    EntryPoint: std::ptr::null_mut(),
                };
                // SAFETY: out-struct with its size.
                unsafe {
                    K32GetModuleInformation(
                        GetCurrentProcess(),
                        h,
                        &mut mi,
                        std::mem::size_of::<MODULEINFO>() as u32,
                    );
                }
                let origin = classify(&path, &windows_dir, &app_dir);
                Module {
                    path,
                    base: mi.lpBaseOfDll as usize,
                    size: mi.SizeOfImage as usize,
                    origin,
                }
            })
            .collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn install_native_filter() {}

    pub fn loaded_modules() -> Vec<Module> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(path: &str, base: usize, size: usize, origin: Origin) -> Module {
        Module {
            path: path.to_owned(),
            base,
            size,
            origin,
        }
    }

    #[test]
    fn classify_by_directory_case_insensitively() {
        let win = r"C:\WINDOWS";
        let app = r"C:\Apps\Chappa";
        assert_eq!(
            classify(r"c:\windows\system32\ntdll.dll", win, app),
            Origin::System
        );
        assert_eq!(
            classify(r"C:\Windows\WinSxS\x\comctl32.dll", win, app),
            Origin::System
        );
        assert_eq!(
            classify(r"C:\Apps\Chappa\chappa-ai-desktop.exe", win, app),
            Origin::App
        );
        assert_eq!(
            classify(r"C:\Program Files\Vendor\hook64.dll", win, app),
            Origin::Other
        );
        // A sibling directory that merely shares the prefix is not inside.
        assert_eq!(classify(r"C:\Windows2\evil.dll", win, app), Origin::Other);
        assert_eq!(classify(r"C:\Apps\ChappaX\x.dll", win, app), Origin::Other);
        // Unknown directories never match anything.
        assert_eq!(classify(r"C:\x\y.dll", "", ""), Origin::Other);
    }

    #[test]
    fn crash_report_names_the_faulting_module_and_lists_foreign_ones_first() {
        let modules = vec![
            module(
                r"C:\Windows\System32\ntdll.dll",
                0x7ff0_0000,
                0x1000,
                Origin::System,
            ),
            module(
                r"C:\Apps\Chappa\chappa-ai-desktop.exe",
                0x1000_0000,
                0x2000,
                Origin::App,
            ),
            module(
                r"C:\Program Files\Vendor\hook64.dll",
                0x2000_0000,
                0x1000,
                Origin::Other,
            ),
        ];
        let report = render_crash_report(0xc000_0005, 0x2000_0123, &modules);
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[0], "exception 0xc0000005 at 0x0000000020000123");
        assert_eq!(lines[1], "in hook64.dll+0x123");
        assert_eq!(lines[3], "foreign modules: 1");
        assert_eq!(lines[4], r"  C:\Program Files\Vendor\hook64.dll");
        assert!(report.contains("all modules: 3"));
        assert!(report.contains("system C:\\Windows\\System32\\ntdll.dll"));

        // The boundary: one past the end of a module is not inside it.
        let outside = render_crash_report(0xc000_0005, 0x2000_1000, &modules);
        assert!(outside
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("in <no loaded module>"));
    }

    #[cfg(windows)]
    #[test]
    fn inventory_sees_this_process() {
        let modules = loaded_modules();
        let exe = std::env::current_exe().unwrap();
        assert!(modules.iter().any(|m| {
            m.origin == Origin::App && m.path.eq_ignore_ascii_case(&exe.to_string_lossy())
        }));
        assert!(modules
            .iter()
            .any(|m| m.origin == Origin::System
                && file_name(&m.path).eq_ignore_ascii_case("ntdll.dll")));
        assert!(modules.iter().all(|m| m.size > 0 && m.base > 0));
    }

    /// The other half of the end-to-end test below: when the probe
    /// variable is set, this process installs the handler and crashes on
    /// purpose. Without it the test is a no-op.
    #[cfg(windows)]
    #[test]
    fn crash_probe_child() {
        let Ok(dir) = std::env::var("CHAPPA_CRASH_PROBE_DIR") else {
            return;
        };
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
        };
        // No OS crash dialog and no OS dump for a deliberate crash.
        unsafe {
            SetErrorMode(SEM_NOGPFAULTERRORBOX | SEM_FAILCRITICALERRORS);
        }
        install(PathBuf::from(dir));
        // SAFETY: intentionally invalid; the whole point is the fault.
        unsafe {
            std::ptr::write_volatile(8 as *mut u8, 1);
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_crash_writes_dump_and_report() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "crash::tests::crash_probe_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHAPPA_CRASH_PROBE_DIR", dir.path())
            .env_remove("RUST_BACKTRACE")
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "the probe child must die of its access violation"
        );

        let mut dumps = Vec::new();
        let mut reports = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("dmp") => dumps.push(path),
                Some("txt") => reports.push(path),
                _ => {}
            }
        }
        assert_eq!(dumps.len(), 1, "one minidump: {dumps:?}");
        assert!(std::fs::metadata(&dumps[0]).unwrap().len() > 4096);
        // modules.txt from install() plus the crash report.
        assert_eq!(reports.len(), 2, "modules.txt + crash report: {reports:?}");
        let crash = reports
            .iter()
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("crash-")
            })
            .expect("crash-*.txt");
        let text = std::fs::read_to_string(crash).unwrap();
        assert!(text.starts_with("exception 0xc0000005 at 0x"), "{text}");
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(text.contains(&format!("in {exe_name}+0x")), "{text}");
        assert!(text.contains("foreign modules:"), "{text}");
        let inventory = std::fs::read_to_string(dir.path().join("modules.txt")).unwrap();
        assert!(inventory.contains("ntdll.dll"), "{inventory}");
    }
}
