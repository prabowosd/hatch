//! The running server's LSIAPI table, read through the layout its core actually uses.
//! CyberPanel's core inserts three php members after `set_req_env`, so every later slot shifts.
//! With the stock layout `set_resp_header` lands on `flush` and recurses until SIGSEGV.

use crate::ffi::{iovec, lsi_session_t};
use std::ffi::{c_char, c_int, c_uint};
use std::sync::OnceLock;

/// Index of the last member both layouts agree on (`set_req_env`).
const SHIFT_AFTER: usize = 48;
const CYBERPANEL_SHIFT: usize = 3;
/// Prefix shared by CyberPanel's ABI markers; the suffix is the layout version.
const MARKER: &[u8] = b"cyberpanel_lsi_api_abi_";

extern "C" {
    fn hatch_api() -> *const usize;
    /// 1 when the core exports CyberPanel's v1 ABI marker.
    fn hatch_api_marker_v1() -> c_int;
}

/// The members hatch calls, with their stock `lsi_api_t` indices.
pub struct Api {
    pub log: unsafe extern "C" fn(*const lsi_session_t, c_int, *const c_char, ...),
    pub get_req_uri: unsafe extern "C" fn(*const lsi_session_t, *mut c_int) -> *const c_char,
    pub get_client_ip: unsafe extern "C" fn(*const lsi_session_t, *mut c_int) -> *const c_char,
    pub get_req_var_by_id: unsafe extern "C" fn(*const lsi_session_t, c_int, *mut c_char, c_int) -> c_int,
    pub get_req_env: unsafe extern "C" fn(*const lsi_session_t, *const c_char, c_uint, *mut c_char, c_int) -> c_int,
    pub set_req_env: unsafe extern "C" fn(*const lsi_session_t, *const c_char, c_uint, *const c_char, c_int),
    pub get_req_header: unsafe extern "C" fn(*const lsi_session_t, *const c_char, c_int, *mut c_int) -> *const c_char,
    pub get_uri_file_path: unsafe extern "C" fn(*const lsi_session_t, *const c_char, c_int, *mut c_char, c_int) -> c_int,
    pub set_uri_qs: unsafe extern "C" fn(*const lsi_session_t, c_int, *const c_char, c_int, *const c_char, c_int) -> c_int,
    pub set_status_code: unsafe extern "C" fn(*const lsi_session_t, c_int),
    pub get_status_code: unsafe extern "C" fn(*const lsi_session_t) -> c_int,
    pub set_resp_header: unsafe extern "C" fn(*const lsi_session_t, c_uint, *const c_char, c_int, *const c_char, c_int, c_int) -> c_int,
    pub get_resp_header: unsafe extern "C" fn(*const lsi_session_t, c_uint, *const c_char, c_int, *mut iovec, c_int) -> c_int,
    pub remove_resp_header: unsafe extern "C" fn(*const lsi_session_t, c_uint, *const c_char, c_int) -> c_int,
    pub get_req_handler_type: unsafe extern "C" fn(*const lsi_session_t) -> *const c_char,
    /// CyberPanel's core only: the three php members sit right after `set_req_env`.
    pub set_php_config_value: Option<unsafe extern "C" fn(*const lsi_session_t, *const c_char, *const c_char, c_int) -> c_int>,
    pub set_php_config_flag: Option<unsafe extern "C" fn(*const lsi_session_t, *const c_char, c_int, c_int) -> c_int>,
}

/// Why the table could not be read, for the log line in `init`.
#[derive(Clone, Copy)]
pub enum Unsupported {
    NoTable,
    /// A CyberPanel core whose marker is a layout version hatch does not implement.
    UnknownLayout,
    /// The core left a member hatch needs unset.
    MissingMember(&'static str),
}

impl Unsupported {
    pub fn describe(&self) -> String {
        match self {
            Self::NoTable => "the server exposes no LSIAPI table".into(),
            Self::UnknownLayout => "unrecognised CyberPanel LSIAPI layout; hatch stays idle".into(),
            Self::MissingMember(m) => format!("the server's LSIAPI table has no {m}"),
        }
    }
}

impl Api {
    unsafe fn read(table: *const usize, shift: usize) -> Result<Self, Unsupported> {
        Ok(Self {
            log: entry(table, 1, shift, "log")?,
            get_req_uri: entry(table, 27, shift, "get_req_uri")?,
            get_client_ip: entry(table, 44, shift, "get_client_ip")?,
            get_req_var_by_id: entry(table, 46, shift, "get_req_var_by_id")?,
            get_req_env: entry(table, 47, shift, "get_req_env")?,
            set_req_env: entry(table, 48, shift, "set_req_env")?,
            get_req_header: entry(table, 24, shift, "get_req_header")?,
            get_uri_file_path: entry(table, 50, shift, "get_uri_file_path")?,
            set_uri_qs: entry(table, 51, shift, "set_uri_qs")?,
            set_status_code: entry(table, 65, shift, "set_status_code")?,
            get_status_code: entry(table, 66, shift, "get_status_code")?,
            set_resp_header: entry(table, 77, shift, "set_resp_header")?,
            get_resp_header: entry(table, 80, shift, "get_resp_header")?,
            remove_resp_header: entry(table, 84, shift, "remove_resp_header")?,
            get_req_handler_type: entry(table, 89, shift, "get_req_handler_type")?,
            // Only CyberPanel's core carries these, at the slots it inserts after set_req_env.
            set_php_config_value: optional(table, 49, shift),
            set_php_config_flag: optional(table, 50, shift),
        })
    }
}

/// A member a core may not have. CyberPanel's php members sit at fixed absolute slots.
unsafe fn optional<F>(table: *const usize, index: usize, shift: usize) -> Option<F> {
    if shift == 0 {
        return None;
    }
    let addr = *table.add(index);
    (addr != 0).then(|| std::mem::transmute_copy(&addr))
}

/// Where a stock index lands once the core's own members are accounted for.
fn slot(index: usize, shift: usize) -> usize {
    if index > SHIFT_AFTER {
        index + shift
    } else {
        index
    }
}

unsafe fn entry<F>(table: *const usize, index: usize, shift: usize, name: &'static str) -> Result<F, Unsupported> {
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>(), "{name} is not one table slot");
    let addr = *table.add(slot(index, shift));
    if addr == 0 {
        return Err(Unsupported::MissingMember(name));
    }
    Ok(std::mem::transmute_copy(&addr))
}

/// The table for this process, resolved once per worker.
pub fn get() -> Result<&'static Api, Unsupported> {
    static API: OnceLock<Result<Api, Unsupported>> = OnceLock::new();
    API.get_or_init(detect).as_ref().map_err(|e| e.clone())
}

fn detect() -> Result<Api, Unsupported> {
    let table = unsafe { hatch_api() };
    if table.is_null() {
        return Err(Unsupported::NoTable);
    }
    let (layout, shift) = match layout() {
        Layout::Stock => ("stock", 0),
        Layout::CyberPanelV1 => ("cyberpanel", CYBERPANEL_SHIFT),
        Layout::Unknown => return Err(Unsupported::UnknownLayout),
    };
    let api = unsafe { Api::read(table, shift) }?;
    LAYOUT.set(layout).ok();
    Ok(api)
}

/// The layout hatch picked, for the load line in `init`.
pub fn layout_name() -> &'static str {
    LAYOUT.get().copied().unwrap_or("unknown")
}

static LAYOUT: OnceLock<&'static str> = OnceLock::new();

enum Layout {
    Stock,
    CyberPanelV1,
    Unknown,
}

fn layout() -> Layout {
    if unsafe { hatch_api_marker_v1() } == 1 {
        return Layout::CyberPanelV1;
    }
    match exe_marker() {
        Some(1) => Layout::CyberPanelV1,
        // The symbol can be localised, so the bytes decide; an unknown version stays idle.
        Some(_) => Layout::Unknown,
        None => Layout::Stock,
    }
}

/// The layout version in the server binary's ABI marker, when it carries one.
fn exe_marker() -> Option<u32> {
    let mut exe = std::fs::File::open("/proc/self/exe").ok()?;
    // Read in chunks with an overlap so a marker across a boundary is still found.
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK + MARKER.len()];
    let mut filled = 0;
    loop {
        let n = std::io::Read::read(&mut exe, &mut buf[filled..]).ok()?;
        let end = filled + n;
        if let Some(at) = buf[..end].windows(MARKER.len()).position(|w| w == MARKER) {
            let digits: Vec<u8> = buf[at + MARKER.len()..end].iter().copied().take_while(u8::is_ascii_digit).collect();
            return std::str::from_utf8(&digits).ok()?.parse().ok();
        }
        if n == 0 {
            return None;
        }
        if end < MARKER.len() {
            return None;
        }
        buf.copy_within(end - MARKER.len()..end, 0);
        filled = MARKER.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyberpanel_members_shift_only_after_set_req_env() {
        assert_eq!(slot(1, 0), 1);
        assert_eq!(slot(1, 3), 1);
        assert_eq!(slot(46, 3), 46);
        assert_eq!(slot(48, 3), 48);
        assert_eq!(slot(50, 3), 53);
        assert_eq!(slot(89, 3), 92);
        assert_eq!(slot(50, 0), 50);
    }

    fn table() -> Vec<usize> {
        (0..140).map(|i| i + 1).collect()
    }

    #[test]
    fn reads_the_stock_layout() {
        let t = table();
        let api = unsafe { Api::read(t.as_ptr(), 0) }.ok().expect("stock layout");
        assert_eq!(api.log as usize, t[1]);
        assert_eq!(api.get_uri_file_path as usize, t[50]);
        assert_eq!(api.set_status_code as usize, t[65]);
        assert_eq!(api.set_resp_header as usize, t[77]);
        assert_eq!(api.get_req_handler_type as usize, t[89]);
    }

    #[test]
    fn reads_the_cyberpanel_layout() {
        let stock = table();
        let mut shifted = vec![0usize; 140];
        // Three php config members go in after set_req_env, pushing the rest along.
        shifted[..=SHIFT_AFTER].copy_from_slice(&stock[..=SHIFT_AFTER]);
        shifted[SHIFT_AFTER + 1..SHIFT_AFTER + 4].copy_from_slice(&[9001, 9002, 9003]);
        shifted[SHIFT_AFTER + 4..].copy_from_slice(&stock[SHIFT_AFTER + 1..137]);

        let api = unsafe { Api::read(shifted.as_ptr(), CYBERPANEL_SHIFT) }.ok().expect("cyberpanel layout");
        assert_eq!(api.log as usize, stock[1]);
        assert_eq!(api.get_req_var_by_id as usize, stock[46]);
        assert_eq!(api.get_uri_file_path as usize, stock[50]);
        assert_eq!(api.set_status_code as usize, stock[65]);
        assert_eq!(api.set_resp_header as usize, stock[77]);
        assert_eq!(api.get_resp_header as usize, stock[80]);
        assert_eq!(api.remove_resp_header as usize, stock[84]);
        assert_eq!(api.get_req_handler_type as usize, stock[89]);
    }

    #[test]
    fn reads_the_cyberpanel_php_members() {
        let stock = table();
        let mut shifted = vec![0usize; 140];
        shifted[..=SHIFT_AFTER].copy_from_slice(&stock[..=SHIFT_AFTER]);
        // The php members land at the slots CyberPanel inserts them into.
        shifted[49] = 9001;
        shifted[50] = 9002;
        shifted[51] = 9003;
        shifted[SHIFT_AFTER + 4..].copy_from_slice(&stock[SHIFT_AFTER + 1..137]);
        let api = unsafe { Api::read(shifted.as_ptr(), CYBERPANEL_SHIFT) }.ok().expect("cyberpanel layout");
        assert_eq!(api.set_php_config_value.map(|f| f as usize), Some(9001));
        assert_eq!(api.set_php_config_flag.map(|f| f as usize), Some(9002));
        // The stock layout has no php members, whatever sits at those offsets.
        let stock_api = unsafe { Api::read(stock.as_ptr(), 0) }.ok().expect("stock layout");
        assert!(stock_api.set_php_config_value.is_none() && stock_api.set_php_config_flag.is_none());
    }

    #[test]
    fn an_unset_member_is_reported() {
        let mut t = table();
        t[53] = 0;
        match unsafe { Api::read(t.as_ptr(), CYBERPANEL_SHIFT) } {
            Err(Unsupported::MissingMember(name)) => assert_eq!(name, "get_uri_file_path"),
            _ => panic!("expected a missing member"),
        }
    }
}
