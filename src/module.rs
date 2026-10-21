//! The OpenLiteSpeed side: hook registration, .htaccess discovery and caching.
//!
//! Every callback runs inside catch_unwind and fails open: a bug in hatch must
//! never take the web server down, so an error just skips the .htaccess rules.

use crate::api::{self, Api};
use crate::ffi::*;
use crate::parse::parse;
use crate::rules::{decide, Decision, HeaderAction, Request, Rules};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn api() -> Option<&'static Api> {
    api::get().ok()
}

fn log(session: *const lsi_session_t, level: u32, text: &str) {
    if let (Some(api), Ok(msg)) = (api(), std::ffi::CString::new(format!("[hatch] {text}"))) {
        unsafe { (api.log)(session, level as c_int, c"%s\n".as_ptr(), msg.as_ptr()) };
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

fn read(f: unsafe extern "C" fn(*const lsi_session_t, *mut c_int) -> *const c_char, s: *const lsi_session_t) -> Option<String> {
    let mut len: c_int = 0;
    let ptr = unsafe { f(s, &mut len) };
    if ptr.is_null() || len <= 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn doc_root(api: &Api, s: *const lsi_session_t) -> Option<PathBuf> {
    let mut buf = [0u8; BUF];
    let n = unsafe { (api.get_req_var_by_id)(s, LSI_REQ_VARIABLE_LSI_VAR_DOC_ROOT as c_int, buf.as_mut_ptr() as *mut c_char, BUF as c_int) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).trim_end_matches('/').to_string()))
}

fn file_path(api: &Api, s: *const lsi_session_t, uri: &str) -> Option<PathBuf> {
    let mut buf = [0u8; BUF];
    let n = unsafe { (api.get_uri_file_path)(s, uri.as_ptr() as *const c_char, uri.len() as c_int, buf.as_mut_ptr() as *mut c_char, BUF as c_int) };
    (n > 0).then(|| {
        let end = (n as usize).min(BUF);
        let end = buf[..end].iter().position(|&b| b == 0).unwrap_or(end);
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

/// The file a URI names under the docroot. Rejects anything that is not a plain path inside it.
fn map_uri(root: &Path, uri: &str) -> Option<PathBuf> {
    let clean = uri.split(['?', '#']).next().unwrap_or("");
    if !clean.starts_with('/') || clean.contains('\0') {
        return None;
    }
    let mut path = root.to_path_buf();
    for part in clean.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

/// The decision for this request, or None when no .htaccess applies.
fn evaluate(s: *const lsi_session_t) -> Option<Decision> {
    let api = api()?;
    let uri = read(api.get_req_uri, s);
    let root = doc_root(api, s);
    // get_uri_file_path returns nonzero when it cannot map the URI, so fall back to the docroot.
    let path = uri.as_deref().and_then(|u| file_path(api, s, u)).filter(|p| !p.as_os_str().is_empty())
        .or_else(|| Some(map_uri(root.as_deref()?, uri.as_deref()?)?));
    // If the server's path is not inside the docroot, fall back to the docroot mapping.
    let path = match (&root, &path, uri.as_deref()) {
        (Some(r), Some(p), Some(u)) if p.strip_prefix(r).is_err() => map_uri(r, u)?,
        (_, Some(p), _) => p.clone(),
        _ => return None,
    };
    trace(s, || format!("uri={uri:?} doc_root={root:?} file={path:?}"));
    let (uri, root, path) = (uri?, root?, path);
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
    let method = req_var(api, s, LSI_REQ_VARIABLE_LSI_VAR_REQ_METHOD as c_int);
    let header = |name: &str| req_header(api, s, name);
    let req = Request { uri: &uri, file_name: &file_name, method: method.as_deref().unwrap_or("GET"), client, header: &header };
    let refs: Vec<&Rules> = chain.iter().map(Rc::as_ref).collect();
    Some(decide(&refs, &req))
}

/// One request header by name, as SetEnvIf and Header env= need it.
fn req_header(api: &Api, s: *const lsi_session_t, name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut len: c_int = 0;
    let ptr = unsafe { (api.get_req_header)(s, name.as_ptr(), name.as_bytes().len() as c_int, &mut len) };
    if ptr.is_null() || len <= 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// A server variable that is always a short string, like the request method.
fn req_var(api: &Api, s: *const lsi_session_t, id: c_int) -> Option<String> {
    let mut buf = [0u8; 64];
    let n = unsafe { (api.get_req_var_by_id)(s, id, buf.as_mut_ptr() as *mut c_char, buf.len() as c_int) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Whether the request arrived over TLS, for `SSLRequireSSL`.
fn is_https(api: &Api, s: *const lsi_session_t) -> bool {
    req_var(api, s, LSI_REQ_VARIABLE_LSI_VAR_HTTPS as c_int).is_some_and(|v| v.eq_ignore_ascii_case("on"))
}

fn guard(f: impl FnOnce() -> c_int + std::panic::UnwindSafe) -> c_int {
    std::panic::catch_unwind(f).unwrap_or(LSI_RETCODE_LSI_OK as c_int)
}

unsafe extern "C" fn on_uri_map(param: *mut lsi_param_t) -> c_int {
    guard(|| {
        let s = unsafe { (*param).session };
        let (Some(api), Some(mut d)) = (api(), evaluate(s)) else { return LSI_RETCODE_LSI_OK as c_int };
        // Env and php config belong to the request, so they are set before anything else uses them.
        apply_env(api, s, &d);
        apply_php(api, s, &d);
        // SSLRequireSSL denies a request that did not arrive over TLS.
        if d.ssl_require && !is_https(api, s) {
            d.denied = true;
        }
        if let Some((status, target)) = d.redirect {
            // Status, Location and an early end: the same path 410 takes, and it covers 308 too.
            unsafe { (api.set_status_code)(s, status as c_int) };
            if let Some(t) = target {
                set_header(api, s, "Location", &t, LSI_HEADER_OP_LSI_HEADEROP_SET);
            }
            return LSI_RETCODE_LSI_ERROR as c_int;
        }
        // A denied request goes to its ErrorDocument when one is configured, else the hook denies.
        let errordoc = apply_errordocument(api, s, &d);
        if d.denied {
            return if errordoc { LSI_RETCODE_LSI_OK as c_int } else { LSI_RETCODE_LSI_DENY as c_int };
        }
        if errordoc {
            return LSI_RETCODE_LSI_OK as c_int;
        }
        if d.indexes == Some(false) {
            // A directory with no index file would get a listing; Apache answers 403 instead.
            if let Some(code) = deny_directory_listing(api, s) {
                unsafe { (api.set_status_code)(s, code) };
                return LSI_RETCODE_LSI_ERROR as c_int;
            }
        }
        LSI_RETCODE_LSI_OK as c_int
    })
}

/// 403 when this request would produce a directory listing, otherwise None.
fn deny_directory_listing(api: &Api, s: *const lsi_session_t) -> Option<c_int> {
    let uri = read(api.get_req_uri, s)?;
    if !uri.ends_with('/') {
        return None;
    }
    let root = doc_root(api, s)?;
    let dir = file_path(api, s, &uri).filter(|p| !p.as_os_str().is_empty())
        .or_else(|| map_uri(&root, &uri))?;
    const INDEX_FILES: [&str; 3] = ["index.html", "index.php", "index.htm"];
    let has_index = INDEX_FILES.iter().any(|name| dir.join(name).is_file());
    (!has_index && dir.is_dir()).then_some(403)
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

fn content_type(api: &Api, s: *const lsi_session_t) -> String {
    let mut iov = iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
    let n = unsafe { (api.get_resp_header)(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_CONTENT_TYPE, std::ptr::null(), 0, &mut iov, 1) };
    if n < 1 || iov.iov_base.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn remove_header(api: &Api, s: *const lsi_session_t, name: &str) {
    unsafe { (api.remove_resp_header)(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_UNKNOWN, name.as_ptr() as *const c_char, name.len() as c_int) };
}

/// Like mod_expires: a dynamic response that set its own Cache-Control or Expires keeps it.
/// Static files have no app behind them, so .htaccess may replace the server default.
fn app_set_cache(api: &Api, s: *const lsi_session_t) -> bool {
    // Handler type names come from the server: "static" for plain files, "lsapi", "proxy" and so on otherwise.
    let ptr = unsafe { (api.get_req_handler_type)(s) };
    let is_static = ptr.is_null() || unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes().eq_ignore_ascii_case(b"static");
    if is_static {
        return false;
    }
    [LSI_RESP_HEADER_ID_LSI_RSPHDR_CACHE_CTRL, LSI_RESP_HEADER_ID_LSI_RSPHDR_EXPIRES].iter().any(|&id| {
        let mut iov = iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        unsafe { (api.get_resp_header)(s, id, std::ptr::null(), 0, &mut iov, 1) > 0 }
    })
}

fn set_header(api: &Api, s: *const lsi_session_t, name: &str, value: &str, op: u32) {
    unsafe {
        (api.set_resp_header)(s, LSI_RESP_HEADER_ID_LSI_RSPHDR_UNKNOWN, name.as_ptr() as *const c_char, name.len() as c_int, value.as_ptr() as *const c_char, value.len() as c_int, op as c_int);
    }
}

/// SetEnv, SetEnvIf and UnsetEnv, so the backend and rewrite rules see the same environment.
fn apply_env(api: &Api, s: *const lsi_session_t, d: &Decision) {
    for (name, value) in &d.env {
        unsafe { (api.set_req_env)(s, name.as_ptr() as *const c_char, name.len() as u32, value.as_ptr() as *const c_char, value.len() as c_int) };
    }
    for name in &d.env_unset {
        // An empty value is how the server clears a request environment variable.
        unsafe { (api.set_req_env)(s, name.as_ptr() as *const c_char, name.len() as u32, std::ptr::null(), 0) };
    }
}

/// php_value and php_flag, through the members only CyberPanel's core carries.
fn apply_php(api: &Api, s: *const lsi_session_t, d: &Decision) {
    if d.php.values.is_empty() && d.php.flags.is_empty() {
        return;
    }
    if api.set_php_config_value.is_none() || api.set_php_config_flag.is_none() {
        log(s, LSI_LOG_LEVEL_LSI_LOG_WARN, "php_value and php_flag need CyberPanel's OpenLiteSpeed core; ignored");
        return;
    }
    for (name, value) in &d.php.values {
        let (Ok(name), Ok(value)) = (std::ffi::CString::new(name.as_str()), std::ffi::CString::new(value.as_str())) else { continue };
        // The core formats "name value" itself; the last argument selects the PHP config type.
        unsafe { (api.set_php_config_value.unwrap())(s, name.as_ptr(), value.as_ptr(), PHP_CONFIG_TYPE) };
    }
    for (name, on) in &d.php.flags {
        let Ok(name) = std::ffi::CString::new(name.as_str()) else { continue };
        unsafe { (api.set_php_config_flag.unwrap())(s, name.as_ptr(), *on as c_int, PHP_CONFIG_TYPE) };
    }
}

/// `parsePHPConfig` reads 2 as the per-request PHP config type.
const PHP_CONFIG_TYPE: c_int = 2;

/// ErrorDocument: send a denied or missing page to the configured one and keep its status.
///
/// LSIAPI only lets a handler write a body, so a filter cannot replace the error page. An
/// internal redirect serves the configured file; the status is set before it starts.
fn apply_errordocument(api: &Api, s: *const lsi_session_t, d: &Decision) -> bool {
    if d.errordocs.is_empty() {
        return false;
    }
    let Some(uri) = read(api.get_req_uri, s) else { return false };
    let Some(root) = doc_root(api, s) else { return false };
    let path = file_path(api, s, &uri).filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| map_uri(&root, &uri).unwrap_or_else(|| root.clone()));
    let code = if d.denied {
        403
    } else if !path.exists() && !uri.ends_with('/') {
        404
    } else {
        return false;
    };
    let Some(doc) = d.errordocs.iter().find(|doc| doc.code == code) else { return false };
    // The target runs through this hook again; redirecting it to itself would loop.
    if doc.target == uri {
        return false;
    }
    unsafe {
        (api.set_status_code)(s, code as c_int);
        (api.set_uri_qs)(s, LSI_URL_OP_LSI_URL_REDIRECT_INTERNAL as c_int, doc.target.as_ptr() as *const c_char, doc.target.len() as c_int, std::ptr::null(), 0);
    }
    true
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
        // AddDefaultCharset only fills in a missing charset; an explicit one wins.
        if let Some(charset) = d.default_charset.as_deref().filter(|c| !c.is_empty()) {
            let ct = content_type(api, s);
            if !ct.is_empty() && !ct.to_ascii_lowercase().contains("charset=") {
                set_header(api, s, "Content-Type", &format!("{ct}; charset={charset}"), LSI_HEADER_OP_LSI_HEADEROP_SET);
            }
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
    match api::get() {
        Ok(_) => log(
            std::ptr::null(),
            LSI_LOG_LEVEL_LSI_LOG_INFO,
            &format!("module v{} loaded, {} LSIAPI layout", env!("CARGO_PKG_VERSION"), api::layout_name()),
        ),
        Err(e) => log(std::ptr::null(), LSI_LOG_LEVEL_LSI_LOG_WARN, &format!("module v{} idle: {}", env!("CARGO_PKG_VERSION"), e.describe())),
    }
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
    fn uri_mapping_stays_inside_the_docroot() {
        use super::map_uri;
        use std::path::{Path, PathBuf};
        let root = Path::new("/home/site/public_html");
        assert_eq!(map_uri(root, "/a/b.css?x=1"), Some(PathBuf::from("/home/site/public_html/a/b.css")));
        assert_eq!(map_uri(root, "/"), Some(PathBuf::from("/home/site/public_html")));
        assert_eq!(map_uri(root, "//a/./b/"), Some(PathBuf::from("/home/site/public_html/a/b")));
        assert_eq!(map_uri(root, "/a/../../etc/passwd"), None);
        assert_eq!(map_uri(root, "relative"), None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(UNIX_EPOCH + Duration::from_secs(1_792_000_000)), "Wed, 14 Oct 2026 17:46:40 GMT");
    }
}
