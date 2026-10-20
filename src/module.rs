//! The OpenLiteSpeed side: hook registration, .htaccess discovery and caching.
//!
//! Every callback runs inside catch_unwind and fails open: a bug in hatch must
//! never take the web server down, so an error just skips the .htaccess rules.

use crate::ffi::*;
use crate::parse::parse;
use crate::rules::{decide, Decision, HeaderAction, Rules};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

extern "C" {
    fn hatch_api() -> *const lsi_api_t;
}

const MAX_HTACCESS: u64 = 256 * 1024;
const MAX_CACHE: usize = 4096;
const BUF: usize = 4096;

struct Cached {
    mtime: i64,
    size: u64,
    rules: Option<Rc<Rules>>,
}

thread_local! {
    // Workers are single threaded event loops, so a per thread cache needs no locks.
    static CACHE: RefCell<HashMap<PathBuf, Cached>> = RefCell::new(HashMap::new());
}

fn api() -> Option<&'static lsi_api_t> {
    unsafe { hatch_api().as_ref() }
}

fn log(session: *const lsi_session_t, level: u32, text: &str) {
    if let (Some(api), Ok(msg)) = (api(), std::ffi::CString::new(format!("[hatch] {text}"))) {
        if let Some(f) = api.log {
            unsafe { f(session, level as c_int, c"%s\n".as_ptr(), msg.as_ptr()) };
        }
    }
}

/// Parsed rules for one .htaccess, reread only when its mtime or size changes.
fn load(path: &Path, session: *const lsi_session_t) -> Option<Rc<Rules>> {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let (mtime, size) = (meta.mtime(), meta.size());
    if let Some(hit) = CACHE.with(|c| c.borrow().get(path).filter(|e| e.mtime == mtime && e.size == size).map(|e| e.rules.clone())) {
        return hit;
    }
    let rules = if size > MAX_HTACCESS {
        log(session, LSI_LOG_LEVEL_LSI_LOG_WARN, &format!("{} is larger than 256 KB, ignored", path.display()));
        None
    } else {
        match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|s| parse(&s).map_err(|e| e.0)) {
            Ok(nodes) => {
                let rules = Rules::compile(&nodes);
                for note in &rules.notes {
                    log(session, LSI_LOG_LEVEL_LSI_LOG_INFO, &format!("{}: {note}", path.display()));
                }
                Some(Rc::new(rules))
            }
            Err(e) => {
                log(session, LSI_LOG_LEVEL_LSI_LOG_WARN, &format!("{}: {e}, ignored", path.display()));
                None
            }
        }
    };
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= MAX_CACHE {
            c.clear();
        }
        c.insert(path.to_path_buf(), Cached { mtime, size, rules: rules.clone() });
    });
    rules
}

fn read(f: Option<unsafe extern "C" fn(*const lsi_session_t, *mut c_int) -> *const c_char>, s: *const lsi_session_t) -> Option<String> {
    let f = f?;
    let mut len: c_int = 0;
    let ptr = unsafe { f(s, &mut len) };
    if ptr.is_null() || len <= 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn doc_root(api: &lsi_api_t, s: *const lsi_session_t) -> Option<PathBuf> {
    let mut buf = [0u8; BUF];
    let n = unsafe { api.get_req_var_by_id?(s, LSI_REQ_VARIABLE_LSI_VAR_DOC_ROOT as c_int, buf.as_mut_ptr() as *mut c_char, BUF as c_int) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).trim_end_matches('/').to_string()))
}

fn file_path(api: &lsi_api_t, s: *const lsi_session_t, uri: &str) -> Option<PathBuf> {
    let mut buf = [0u8; BUF];
    let n = unsafe { api.get_uri_file_path?(s, uri.as_ptr() as *const c_char, uri.len() as c_int, buf.as_mut_ptr() as *mut c_char, BUF as c_int) };
    (n == 0).then(|| {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(BUF);
        PathBuf::from(String::from_utf8_lossy(&buf[..end]).to_string())
    })
}

thread_local! {
    static DEBUG: bool = Path::new("/usr/local/lsws/conf/hatch.debug").exists();
}

/// Off unless /usr/local/lsws/conf/hatch.debug exists; logs what the server hands hatch.
fn trace(s: *const lsi_session_t, text: impl FnOnce() -> String) {
    if DEBUG.with(|d| *d) {
        log(s, LSI_LOG_LEVEL_LSI_LOG_WARN, &format!("debug: {}", text()));
    }
}

/// The decision for this request, or None when no .htaccess applies.
fn evaluate(s: *const lsi_session_t) -> Option<Decision> {
    let api = api()?;
    let uri = read(api.get_req_uri, s);
    let root = doc_root(api, s);
    let path = uri.as_deref().and_then(|u| file_path(api, s, u));
    trace(s, || format!("uri={uri:?} doc_root={root:?} file={path:?}"));
    let (uri, root, path) = (uri?, root?, path?);
    let rel = path.strip_prefix(&root).ok()?;
    let is_dir = uri.ends_with('/') || path.is_dir();
    let (folders, file_name) = if is_dir {
        (rel.to_path_buf(), String::new())
    } else {
        (rel.parent().map(Path::to_path_buf).unwrap_or_default(), rel.file_name()?.to_string_lossy().into_owned())
    };

    let mut chain: Vec<Rc<Rules>> = Vec::new();
    let mut dir = root.clone();
    if let Some(r) = load(&dir.join(".htaccess"), s) {
        chain.push(r);
    }
    for part in folders.components() {
        dir.push(part);
        if let Some(r) = load(&dir.join(".htaccess"), s) {
            chain.push(r);
        }
    }
    trace(s, || format!("{} .htaccess file(s) apply to {uri}", chain.len()));
    if chain.is_empty() {
        return None;
    }
    let client = read(api.get_client_ip, s).and_then(|ip| ip.parse::<IpAddr>().ok());
    let refs: Vec<&Rules> = chain.iter().map(Rc::as_ref).collect();
    Some(decide(&refs, &uri, &file_name, client))
}

fn guard(f: impl FnOnce() -> c_int + std::panic::UnwindSafe) -> c_int {
    std::panic::catch_unwind(f).unwrap_or(LSI_RETCODE_LSI_OK as c_int)
}

unsafe extern "C" fn on_uri_map(param: *mut lsi_param_t) -> c_int {
    guard(|| {
        let s = unsafe { (*param).session };
        let (Some(api), Some(d)) = (api(), evaluate(s)) else { return LSI_RETCODE_LSI_OK as c_int };
        if let Some((status, target)) = d.redirect {
            // Status, Location and an early end: the same path 410 takes, and it covers 308 too.
            if let Some(code) = api.set_status_code {
                unsafe { code(s, status as c_int) };
            }
            if let Some(t) = target {
                set_header(api, s, "Location", &t, LSI_HEADER_OP_LSI_HEADEROP_SET);
            }
            return LSI_RETCODE_LSI_ERROR as c_int;
        }
        if d.denied {
            return LSI_RETCODE_LSI_DENY as c_int;
        }
        LSI_RETCODE_LSI_OK as c_int
    })
}

fn http_date(at: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant), proleptic Gregorian.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{}, {:02} {} {} {:02}:{:02}:{:02} GMT", DAYS[(days % 7) as usize], day, MONTHS[(month - 1) as usize], year, rem / 3600, rem / 60 % 60, rem % 60)
}

fn content_type(api: &lsi_api_t, s: *const lsi_session_t) -> String {
    let Some(get) = api.get_resp_header else { return String::new() };
    let mut iov = iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
    let n = unsafe { get(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_CONTENT_TYPE, std::ptr::null(), 0, &mut iov, 1) };
    if n < 1 || iov.iov_base.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn remove_header(api: &lsi_api_t, s: *const lsi_session_t, name: &str) {
    if let Some(remove) = api.remove_resp_header {
        unsafe { remove(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_UNKNOWN, name.as_ptr() as *const c_char, name.len() as c_int) };
    }
}

/// Like mod_expires: a dynamic response that set its own Cache-Control or Expires keeps it.
/// Static files have no app behind them, so .htaccess may replace the server default.
fn app_set_cache(api: &lsi_api_t, s: *const lsi_session_t) -> bool {
    // Handler type names come from the server: "static" for plain files, "lsapi", "proxy" and so on otherwise.
    let is_static = api.get_req_handler_type.is_none_or(|f| {
        let ptr = unsafe { f(s) };
        ptr.is_null() || unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes().eq_ignore_ascii_case(b"static")
    });
    if is_static {
        return false;
    }
    let Some(get) = api.get_resp_header else { return false };
    [LSI_RESP_HEADER_ID_LSI_RSPHDR_CACHE_CTRL, LSI_RESP_HEADER_ID_LSI_RSPHDR_EXPIRES].iter().any(|&id| {
        let mut iov = iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        unsafe { get(s, id, std::ptr::null(), 0, &mut iov, 1) > 0 }
    })
}

fn set_header(api: &lsi_api_t, s: *const lsi_session_t, name: &str, value: &str, op: u32) {
    if let Some(set) = api.set_resp_header {
        unsafe {
            set(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_UNKNOWN, name.as_ptr() as *const c_char, name.len() as c_int, value.as_ptr() as *const c_char, value.len() as c_int, op as c_int);
        }
    }
}

unsafe extern "C" fn on_send_resp_header(param: *mut lsi_param_t) -> c_int {
    guard(|| {
        let s = unsafe { (*param).session };
        let (Some(api), Some(d)) = (api(), evaluate(s)) else { return LSI_RETCODE_LSI_OK as c_int };
        // mod_expires runs before mod_headers, so an explicit Header set Cache-Control wins.
        if let Some(secs) = d.expires.seconds_for(&content_type(api, s)).filter(|_| !app_set_cache(api, s)) {
            remove_header(api, s, "Cache-Control");
            remove_header(api, s, "Expires");
            set_header(api, s, "Cache-Control", &format!("max-age={secs}"), LSI_HEADER_OP_LSI_HEADEROP_SET);
            set_header(api, s, "Expires", &http_date(SystemTime::now() + Duration::from_secs(secs)), LSI_HEADER_OP_LSI_HEADEROP_SET);
        }
        for h in &d.headers {
            match h.action {
                HeaderAction::Unset => remove_header(api, s, &h.name),
                HeaderAction::Set => {
                    // Apache "set" replaces every earlier value, including ones the server added itself.
                    remove_header(api, s, &h.name);
                    set_header(api, s, &h.name, &h.value, LSI_HEADER_OP_LSI_HEADEROP_SET);
                }
                HeaderAction::Append => set_header(api, s, &h.name, &h.value, LSI_HEADER_OP_LSI_HEADEROP_APPEND),
                HeaderAction::Merge => set_header(api, s, &h.name, &h.value, LSI_HEADER_OP_LSI_HEADEROP_MERGE),
                HeaderAction::Add => set_header(api, s, &h.name, &h.value, LSI_HEADER_OP_LSI_HEADEROP_ADD),
            }
        }
        LSI_RETCODE_LSI_OK as c_int
    })
}

unsafe extern "C" fn init(_module: *mut lsi_module_t) -> c_int {
    log(std::ptr::null(), LSI_LOG_LEVEL_LSI_LOG_INFO, concat!("module v", env!("CARGO_PKG_VERSION"), " loaded"));
    LSI_RETCODE_LSI_OK as c_int
}

static mut HOOKS: [lsi_serverhook_t; 3] = [
    lsi_serverhook_t { index: LSI_HKPT_LEVEL_LSI_HKPT_URI_MAP as c_int, cb: Some(on_uri_map), priority: 10, flag: LSI_HOOK_FLAG_LSI_FLAG_ENABLED as i16 },
    lsi_serverhook_t { index: LSI_HKPT_LEVEL_LSI_HKPT_SEND_RESP_HEADER as c_int, cb: Some(on_send_resp_header), priority: 50, flag: LSI_HOOK_FLAG_LSI_FLAG_ENABLED as i16 },
    lsi_serverhook_t { index: 0, cb: None, priority: 0, flag: 0 },
];

/// OpenLiteSpeed looks the module up by its name, so the symbol is `hatch`.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static mut hatch: lsi_module_t = lsi_module_t {
    signature: ((0x4C53_494D_i64) << 32) + ((LSIAPI_VERSION_MAJOR as i64) << 16) + LSIAPI_VERSION_MINOR as i64,
    init_pf: Some(init),
    reqhandler: std::ptr::null_mut(),
    config_parser: std::ptr::null_mut(),
    about: concat!("hatch v", env!("CARGO_PKG_VERSION"), " - Apache .htaccess for OpenLiteSpeed\0").as_ptr() as *const c_char,
    serverhook: &raw mut HOOKS as *mut lsi_serverhook_t,
    reserved: [0; 34],
};

#[cfg(test)]
mod tests {
    use super::http_date;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn http_dates() {
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(UNIX_EPOCH + Duration::from_secs(1_792_000_000)), "Wed, 14 Oct 2026 17:46:40 GMT");
    }
}
