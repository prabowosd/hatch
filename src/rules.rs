//! Compile parsed .htaccess nodes into rules and evaluate them for a request.
//!
//! Pure Rust with no server calls, so all behaviour is unit tested. Anything a
//! site owner writes ends up in a response, so names and values are validated
//! and a bad directive is skipped with a note instead of failing the file.

use crate::parse::Node;
use regex::{Regex, RegexBuilder};
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HeaderAction {
    Set,
    Append,
    Merge,
    Add,
    Unset,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeaderOp {
    pub action: HeaderAction,
    pub name: String,
    pub value: String,
    /// `env=name` or `env=!name`; the header applies only when the variable matches.
    pub env: Option<(String, bool)>,
}

/// One `SetEnvIf`-style test. Any test matching sets `set` in the request environment.
#[derive(Debug, Clone)]
pub struct EnvTest {
    /// Header name for `SetEnvIf`, or `None` for `SetEnvIf Request_URI`/`Remote_Host` variables.
    attribute: String,
    pattern: Regex,
    /// `env=!name` style negation: set the variable only when the pattern does not match.
    negate: bool,
    pub set: String,
    pub value: String,
}

#[derive(Debug, Clone, Default)]
pub struct Env {
    pub plain: Vec<(String, String)>,
    pub tests: Vec<EnvTest>,
    /// `UnsetEnv name`
    pub unset: Vec<String>,
}

/// php_value and php_flag, only settable on a core whose table carries the php members.
#[derive(Debug, Clone, Default)]
pub struct Php {
    pub values: Vec<(String, String)>,
    pub flags: Vec<(String, bool)>,
}

/// Apache `ErrorDocument <code> <target>`, where the target is a path, a URL, or quoted text.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorDocument {
    pub code: u16,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct Redirect {
    pattern: Regex,
    target: Option<String>,
    pub status: u16,
    /// Redirect (not RedirectMatch) appends the rest of the path after the prefix.
    keeps_tail: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Net {
    All,
    Cidr(IpAddr, u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Order {
    DenyAllow,
    AllowDeny,
}

#[derive(Debug, Clone, Default)]
pub struct Access {
    order: Option<Order>,
    allow: Vec<Net>,
    deny: Vec<Net>,
    require_granted: Option<bool>,
    require_ip: Vec<Net>,
    require_not_ip: Vec<Net>,
}

#[derive(Debug, Clone, Default)]
pub struct Expires {
    pub active: Option<bool>,
    pub default: Option<u64>,
    pub by_type: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
enum FileMatch {
    Any,
    Name(Regex),
}

#[derive(Debug, Clone)]
pub struct Section {
    files: FileMatch,
    pub headers: Vec<HeaderOp>,
    access: Access,
    pub expires: Expires,
    pub env: Env,
    pub php: Php,
    pub errordocs: Vec<ErrorDocument>,
    pub options_indexes: Option<bool>,
}

/// The compiled form of one .htaccess file.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    sections: Vec<Section>,
    redirects: Vec<Redirect>,
    pub notes: Vec<String>,
}

/// What a request gets after every .htaccess on its path is applied.
#[derive(Debug, Default)]
pub struct Decision {
    pub redirect: Option<(u16, Option<String>)>,
    pub denied: bool,
    pub headers: Vec<HeaderOp>,
    pub expires: Expires,
    pub env: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    pub php: Php,
    pub errordocs: Vec<ErrorDocument>,
    /// `None` leaves the server's own directory listing setting alone.
    pub indexes: Option<bool>,
}

const MAX_REGEX: usize = 1 << 20;

fn regex(pattern: &str, case_insensitive: bool) -> Option<Regex> {
    RegexBuilder::new(pattern).case_insensitive(case_insensitive).size_limit(MAX_REGEX).build().ok()
}

fn header_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn value_ok(value: &str) -> bool {
    value.len() <= 4096 && !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

fn target_ok(target: &str) -> bool {
    value_ok(target)
        && !target.contains(char::is_whitespace)
        && (target.starts_with('/') || target.starts_with("http://") || target.starts_with("https://"))
}

/// Environment variable names reach the server, so keep them to a plain identifier.
fn env_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn php_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

fn parse_net(value: &str) -> Option<Net> {
    if value.eq_ignore_ascii_case("all") {
        return Some(Net::All);
    }
    let (addr, bits) = match value.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (value, None),
    };
    if let Ok(ip) = addr.parse::<IpAddr>() {
        let max = if ip.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        return (bits <= max).then_some(Net::Cidr(ip, bits));
    }
    // Apache 2.2 partial address: "10.1" means 10.1.0.0/16.
    let parts: Vec<&str> = addr.trim_end_matches('.').split('.').collect();
    if bits.is_none() && (1..4).contains(&parts.len()) && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        let mut octets = [0u8; 4];
        for (i, p) in parts.iter().enumerate() {
            octets[i] = p.parse().ok()?;
        }
        return Some(Net::Cidr(IpAddr::from(octets), (parts.len() * 8) as u8));
    }
    None
}

fn net_contains(net: &Net, ip: &IpAddr) -> bool {
    match (net, ip) {
        (Net::All, _) => true,
        (Net::Cidr(IpAddr::V4(n), bits), IpAddr::V4(a)) => {
            let mask = if *bits == 0 { 0 } else { u32::MAX << (32 - *bits as u32) };
            (u32::from(*n) & mask) == (u32::from(*a) & mask)
        }
        (Net::Cidr(IpAddr::V6(n), bits), IpAddr::V6(a)) => {
            let mask = if *bits == 0 { 0 } else { u128::MAX << (128 - *bits as u32) };
            (u128::from(*n) & mask) == (u128::from(*a) & mask)
        }
        (Net::Cidr(IpAddr::V4(n), bits), IpAddr::V6(a)) => {
            a.to_ipv4_mapped().is_some_and(|v4| net_contains(&Net::Cidr(IpAddr::V4(*n), *bits), &IpAddr::V4(v4)))
        }
        _ => false,
    }
}

fn expiry(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if let Some(rest) = text.strip_prefix('a').or_else(|| text.strip_prefix('m')) {
        if let Ok(secs) = rest.parse() {
            return Some(secs);
        }
    }
    let mut words = text.split_whitespace();
    let base = words.next()?;
    if !matches!(base, "access" | "now" | "modification") || words.next()? != "plus" {
        return None;
    }
    let rest: Vec<&str> = words.collect();
    if rest.is_empty() || rest.len() % 2 != 0 {
        return None;
    }
    let mut total: u64 = 0;
    for pair in rest.chunks(2) {
        let n: u64 = pair[0].parse().ok()?;
        let unit = match pair[1].trim_end_matches('s') {
            "year" => 31_536_000,
            "month" => 2_592_000,
            "week" => 604_800,
            "day" => 86_400,
            "hour" => 3_600,
            "minute" => 60,
            "second" => 1,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    Some(total)
}

fn glob_to_regex(glob: &str) -> Option<String> {
    if glob.is_empty() || glob.contains('/') {
        return None;
    }
    let mut out = String::from("^");
    for c in glob.chars() {
        match c {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    Some(out)
}

struct Compiler {
    rules: Rules,
}

impl Compiler {
    fn note(&mut self, line: usize, text: impl Into<String>) {
        self.rules.notes.push(format!("line {line}: {}", text.into()));
    }

    fn walk(&mut self, nodes: &[Node], section: usize) {
        for node in nodes {
            match node {
                Node::Block { name, args, line, children } => self.block(name, args, *line, children, section),
                Node::Directive { name, args, line } => self.directive(&name.to_ascii_lowercase(), args, *line, section),
            }
        }
    }

    fn block(&mut self, name: &str, args: &[String], line: usize, children: &[Node], section: usize) {
        match name.to_ascii_lowercase().as_str() {
            "ifmodule" => {
                // Every module hatch emulates counts as loaded; the negated form is the fallback for its absence.
                if !args.first().is_some_and(|a| a.starts_with('!')) {
                    self.walk(children, section);
                }
            }
            kind @ ("files" | "filesmatch") => {
                let (is_regex, pattern) = match (kind, args) {
                    ("filesmatch", [p, ..]) => (true, p.as_str()),
                    ("files", [tilde, p, ..]) if tilde == "~" => (true, p.as_str()),
                    ("files", [p, ..]) => (false, p.as_str()),
                    _ => return self.note(line, format!("<{name}> needs a pattern")),
                };
                let compiled = if is_regex { regex(pattern, false) } else { glob_to_regex(pattern).and_then(|g| regex(&g, false)) };
                let Some(compiled) = compiled else {
                    return self.note(line, format!("<{name}> pattern {pattern:?} is not supported"));
                };
                self.rules.sections.push(Section {
                    files: FileMatch::Name(compiled),
                    headers: Vec::new(),
                    access: Access::default(),
                    expires: Expires::default(),
                    env: Env::default(),
                    php: Php::default(),
                    errordocs: Vec::new(),
                    options_indexes: None,
                });
                let index = self.rules.sections.len() - 1;
                self.walk(children, index);
            }
            _ => self.note(line, format!("<{name}> is not supported, contents ignored")),
        }
    }

    fn directive(&mut self, name: &str, args: &[String], line: usize, s: usize) {
        match name {
            "header" => self.header(args, line, s),
            "order" => {
                let order = args.join("").to_ascii_lowercase();
                self.rules.sections[s].access.order = match order.as_str() {
                    "deny,allow" => Some(Order::DenyAllow),
                    "allow,deny" => Some(Order::AllowDeny),
                    _ => return self.note(line, "Order must be Deny,Allow or Allow,Deny"),
                };
            }
            "allow" | "deny" => {
                if args.first().map(|a| a.to_ascii_lowercase()) != Some("from".into()) || args.len() < 2 {
                    return self.note(line, format!("{name} expects 'from'"));
                }
                for value in &args[1..] {
                    match parse_net(value) {
                        Some(net) if name == "allow" => self.rules.sections[s].access.allow.push(net),
                        Some(net) => self.rules.sections[s].access.deny.push(net),
                        None => self.note(line, format!("{value:?} is not an IP or subnet, ignored")),
                    }
                }
            }
            "require" => self.require(args, line, s),
            "redirect" | "redirectpermanent" | "redirecttemp" => self.redirect(name, args, line),
            "redirectmatch" => self.redirect_match(args, line),
            "expiresactive" => {
                self.rules.sections[s].expires.active = Some(!args.first().is_some_and(|a| a.eq_ignore_ascii_case("off")));
            }
            "expiresdefault" => match expiry(&args.join(" ")) {
                Some(secs) => self.rules.sections[s].expires.default = Some(secs),
                None => self.note(line, "ExpiresDefault value is not understood"),
            },
            "expiresbytype" => match (args.first(), expiry(&args.get(1..).unwrap_or_default().join(" "))) {
                (Some(mime), Some(secs)) if mime.contains('/') && value_ok(mime) => {
                    self.rules.sections[s].expires.by_type.push((mime.to_ascii_lowercase(), secs));
                }
                _ => self.note(line, "ExpiresByType needs a MIME type and an expiry"),
            },
            "rewriteengine" | "rewriterule" | "rewritecond" | "rewritebase" | "rewriteoptions" => {}
            "setenv" => {
                let Some(name) = args.first().filter(|n| env_name_ok(n)) else {
                    return self.note(line, "SetEnv needs a variable name");
                };
                let value = args.get(1).cloned().unwrap_or_default();
                if !value_ok(&value) {
                    return self.note(line, "SetEnv value is not usable");
                }
                self.rules.sections[s].env.plain.push((name.clone(), value));
            }
            "unsetenv" => {
                let Some(name) = args.first().filter(|n| env_name_ok(n)) else {
                    return self.note(line, "UnsetEnv needs a variable name");
                };
                self.rules.sections[s].env.unset.push(name.clone());
            }
            "setenvif" | "setenvifnocase" | "browsermatch" => self.env_test(name, args, line, s),
            // The LSIAPI table can read a request header but has no call to change one.
            "requestheader" => self.note(line, "RequestHeader is not available in LSIAPI, ignored"),
            "php_value" | "php_admin_value" => {
                if args.len() < 2 {
                    return self.note(line, "php_value needs a name and a value");
                }
                let name = &args[0];
                if !php_name_ok(name) || !value_ok(&args[1..].join(" ")) {
                    return self.note(line, format!("{name} has an unusable name or value"));
                }
                self.rules.sections[s].php.values.push((name.clone(), args[1..].join(" ")));
            }
            "php_flag" | "php_admin_flag" => {
                let (Some(name), Some(value)) = (args.first(), args.get(1)) else {
                    return self.note(line, format!("{name} needs a name and On or Off"));
                };
                let flag = match value.to_ascii_lowercase().as_str() {
                    "on" | "1" => true,
                    "off" | "0" => false,
                    _ => return self.note(line, format!("{name} value must be On or Off")),
                };
                if !php_name_ok(name) {
                    return self.note(line, format!("{name} has an unusable name"));
                }
                self.rules.sections[s].php.flags.push((name.clone(), flag));
            }
            "errordocument" => self.error_document(args, line, s),
            "options" => self.options(args, line, s),
            other => self.note(line, format!("{other} is not supported yet, ignored")),
        }
    }

    fn header(&mut self, args: &[String], line: usize, s: usize) {
        let mut args = args;
        if args.first().is_some_and(|a| a.eq_ignore_ascii_case("always") || a.eq_ignore_ascii_case("onsuccess")) {
            args = &args[1..];
        }
        let (Some(action), Some(name)) = (args.first(), args.get(1)) else {
            return self.note(line, "Header needs an action and a name");
        };
        let action = match action.to_ascii_lowercase().as_str() {
            "set" => HeaderAction::Set,
            "append" => HeaderAction::Append,
            "merge" => HeaderAction::Merge,
            "add" => HeaderAction::Add,
            "unset" => HeaderAction::Unset,
            other => return self.note(line, format!("Header action {other:?} is not supported")),
        };
        let value = args.get(2).cloned().unwrap_or_default();
        let extra = if action == HeaderAction::Unset { 2 } else { 3 };
        let mut env = None;
        for condition in args.iter().skip(extra) {
            // Only the keyword is case-insensitive; the variable name is matched as written.
            if condition.len() >= 4 && condition[..4].eq_ignore_ascii_case("env=") {
                let name = &condition[4..];
                let (name, negated) = match name.strip_prefix('!') {
                    Some(rest) => (rest, true),
                    None => (name, false),
                };
                if !env_name_ok(name) {
                    return self.note(line, format!("Header env {name:?} is not a usable name"));
                }
                env = Some((name.to_string(), negated));
            } else if condition.len() >= 5 && condition[..5].eq_ignore_ascii_case("expr=") {
                return self.note(line, "Header expr= is not supported yet, ignored");
            } else {
                return self.note(line, format!("Header condition {condition:?} is not supported"));
            }
        }
        if !header_name_ok(name) || !value_ok(&value) || (action != HeaderAction::Unset && value.is_empty()) {
            return self.note(line, "Header has an invalid name or value, ignored");
        }
        self.rules.sections[s].headers.push(HeaderOp { action, name: name.clone(), value, env });
    }

    /// `SetEnvIf attribute pattern [env[=value]]`, `BrowserMatch pattern env`.
    fn env_test(&mut self, name: &str, args: &[String], line: usize, s: usize) {
        let (attribute, pattern, rest) = match name {
            "browsermatch" => ("User-Agent", args.first(), args.get(1..).unwrap_or_default()),
            _ => (args.first().map(String::as_str).unwrap_or(""), args.get(1), args.get(2..).unwrap_or_default()),
        };
        let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
            return self.note(line, format!("{name} needs an attribute and a pattern"));
        };
        let Some(compiled) = regex(pattern, name == "setenvifnocase") else {
            return self.note(line, format!("{name} pattern {pattern:?} is not supported"));
        };
        let attribute = attribute.to_ascii_lowercase();
        // `http_` names a request header; store the bare header name so the lookup matches.
        let stored = attribute.strip_prefix("http_").unwrap_or(&attribute).to_string();
        if !matches!(stored.as_str(), "request_uri" | "request_method" | "remote_host" | "remote_addr" | "user-agent" | "referer" | "host") {
            if !header_name_ok(&stored) {
                return self.note(line, format!("{name} attribute {attribute:?} is not supported"));
            }
        }
        let attribute = stored;
        let mut added = false;
        for spec in rest {
            let (set, value, negate) = match spec.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string(), false),
                None => (spec.trim_start_matches('!').to_string(), "1".to_string(), spec.starts_with('!')),
            };
            if !env_name_ok(&set) || !value_ok(&value) {
                self.note(line, format!("{name} variable {spec:?} is not usable"));
                continue;
            }
            self.rules.sections[s].env.tests.push(EnvTest {
                attribute: attribute.clone(),
                pattern: compiled.clone(),
                negate,
                set,
                value,
            });
            added = true;
        }
        if !added && rest.is_empty() {
            self.note(line, format!("{name} needs a variable to set"));
        }
    }

    /// `Options -Indexes` and `Options +Indexes`; other options are reported and ignored.
    fn options(&mut self, args: &[String], line: usize, s: usize) {
        let mut touched = false;
        for option in args {
            let (on, name) = match option.as_bytes().first() {
                Some(b'-') => (false, &option[1..]),
                Some(b'+') => (true, &option[1..]),
                _ => (true, option.as_str()),
            };
            if name.eq_ignore_ascii_case("indexes") {
                self.rules.sections[s].options_indexes = Some(on);
                touched = true;
            } else {
                self.note(line, format!("Options {name} is not supported yet, ignored"));
            }
        }
        if !touched && args.is_empty() {
            self.note(line, "Options needs at least one option");
        }
    }

    /// `ErrorDocument <code> <path>`; URL and text targets need a body hatch cannot write.
    fn error_document(&mut self, args: &[String], line: usize, s: usize) {
        let (Some(code), Some(target)) = (args.first(), args.get(1)) else {
            return self.note(line, "ErrorDocument needs a status code and a target");
        };
        let Ok(code) = code.parse::<u16>() else {
            return self.note(line, "ErrorDocument needs a numeric status code");
        };
        if !(400..600).contains(&code) {
            return self.note(line, format!("ErrorDocument status {code} is not an error"));
        }
        if !target.starts_with('/') {
            return self.note(line, "ErrorDocument with a URL or text target is not supported");
        }
        if !target_ok(target) {
            return self.note(line, "ErrorDocument target is not a usable path");
        }
        self.rules.sections[s].errordocs.push(ErrorDocument { code, target: target.clone() });
    }

    fn require(&mut self, args: &[String], line: usize, s: usize) {
        let lower: Vec<String> = args.iter().map(|a| a.to_ascii_lowercase()).collect();
        let access = &mut self.rules.sections[s].access;
        match lower.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["all", "granted"] => access.require_granted = Some(true),
            ["all", "denied"] => access.require_granted = Some(false),
            ["ip", rest @ ..] | ["not", "ip", rest @ ..] if !rest.is_empty() => {
                let negated = lower[0] == "not";
                let nets: Vec<Net> = args[if negated { 2 } else { 1 }..].iter().filter_map(|v| parse_net(v)).collect();
                if nets.len() != rest.len() {
                    return self.note(line, "Require ip has a value that is not an IP or subnet, ignored");
                }
                let access = &mut self.rules.sections[s].access;
                if negated { access.require_not_ip.extend(nets) } else { access.require_ip.extend(nets) }
            }
            _ => self.note(line, "this Require form is not supported yet, ignored"),
        }
    }

    fn status(&mut self, args: &mut &[String], line: usize, default: u16) -> Option<u16> {
        let code = match args.first().map(|a| a.to_ascii_lowercase()) {
            Some(word) if word == "permanent" => 301,
            Some(word) if word == "temp" => 302,
            Some(word) if word == "seeother" => 303,
            Some(word) if word == "gone" => 410,
            Some(word) if word.chars().all(|c| c.is_ascii_digit()) && !word.is_empty() => word.parse().ok()?,
            _ => return Some(default),
        };
        *args = &args[1..];
        if [301, 302, 303, 307, 308, 410].contains(&code) {
            Some(code)
        } else {
            self.note(line, format!("status {code} is not a redirect, ignored"));
            None
        }
    }

    fn redirect(&mut self, name: &str, args: &[String], line: usize) {
        let mut args = args;
        let default = if name == "redirectpermanent" { 301 } else { 302 };
        let Some(status) = (if name == "redirect" { self.status(&mut args, line, default) } else { Some(default) }) else { return };
        let Some(source) = args.first().filter(|s| s.starts_with('/') && value_ok(s)) else {
            return self.note(line, "Redirect needs a URL path");
        };
        let source = source.trim_end_matches('/');
        let pattern = if source.is_empty() { "^/(.*)$".to_string() } else { format!("^{}(/.*)?$", regex::escape(source)) };
        let target = match (status, args.get(1)) {
            (410, _) => None,
            (_, Some(t)) if target_ok(t) => Some(t.trim_end_matches('/').to_string()),
            _ => return self.note(line, "Redirect target must be a path or an http(s) URL, ignored"),
        };
        let Some(pattern) = regex(&pattern, false) else { return };
        // Redirect is a prefix match, so the rest of the path is always kept, even for "/".
        self.rules.redirects.push(Redirect { pattern, target, status, keeps_tail: true });
    }

    fn redirect_match(&mut self, args: &[String], line: usize) {
        let mut args = args;
        let Some(status) = self.status(&mut args, line, 302) else { return };
        let Some(pattern) = args.first().and_then(|p| regex(p, false)) else {
            return self.note(line, "RedirectMatch pattern is missing or not supported");
        };
        let target = match (status, args.get(1)) {
            (410, _) => None,
            (_, Some(t)) if target_ok(t) => Some(t.clone()),
            _ => return self.note(line, "RedirectMatch target must be a path or an http(s) URL, ignored"),
        };
        self.rules.redirects.push(Redirect { pattern, target, status, keeps_tail: false });
    }
}

impl Rules {
    pub fn compile(nodes: &[Node]) -> Rules {
        let root = Section {
            files: FileMatch::Any,
            headers: Vec::new(),
            access: Access::default(),
            expires: Expires::default(),
            env: Env::default(),
            php: Php::default(),
            errordocs: Vec::new(),
            options_indexes: None,
        };
        let mut compiler = Compiler { rules: Rules { sections: vec![root], ..Default::default() } };
        compiler.walk(nodes, 0);
        compiler.rules
    }
}

impl Access {
    fn is_set(&self) -> bool {
        self.order.is_some() || !self.allow.is_empty() || !self.deny.is_empty() || self.require_granted.is_some()
            || !self.require_ip.is_empty() || !self.require_not_ip.is_empty()
    }

    fn denies(&self, ip: Option<&IpAddr>) -> bool {
        let hit = |nets: &[Net]| ip.is_some_and(|ip| nets.iter().any(|n| net_contains(n, ip)));
        if self.require_granted.is_some() || !self.require_ip.is_empty() || !self.require_not_ip.is_empty() {
            if hit(&self.require_not_ip) {
                return true;
            }
            if !self.require_ip.is_empty() {
                return !hit(&self.require_ip);
            }
            return self.require_granted == Some(false);
        }
        match self.order.unwrap_or(Order::DenyAllow) {
            Order::DenyAllow => hit(&self.deny) && !hit(&self.allow),
            Order::AllowDeny => !hit(&self.allow) || hit(&self.deny),
        }
    }
}

fn substitute(target: &str, caps: &regex::Captures) -> String {
    let mut out = String::with_capacity(target.len());
    let mut chars = target.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            if let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
                chars.next();
                out.push_str(caps.get(d as usize).map_or("", |m| m.as_str()));
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// What the request exposes to `SetEnvIf` and `Header env=`.
pub struct Request<'a> {
    pub uri: &'a str,
    pub file_name: &'a str,
    pub method: &'a str,
    pub client: Option<IpAddr>,
    /// Request headers, by name, for `SetEnvIf X-Forwarded-For ...`.
    pub header: &'a dyn Fn(&str) -> Option<String>,
}

impl<'a> Request<'a> {
    /// The attribute a `SetEnvIf` test reads: a request header, or one of the Apache variables.
    fn attribute(&self, name: &str) -> Option<String> {
        match name {
            "request_uri" => Some(self.uri.to_string()),
            "request_method" => Some(self.method.to_string()),
            "remote_addr" | "remote_host" => self.client.map(|ip| ip.to_string()),
            _ => (self.header)(name),
        }
    }
}

/// Evaluate the .htaccess chain for a request. `chain` runs from the docroot down to the file's folder.
pub fn decide(chain: &[&Rules], req: &Request) -> Decision {
    let mut decision = Decision::default();

    // The deepest folder's redirects win, as with mod_alias in per-directory context.
    'redirects: for rules in chain.iter().rev() {
        for r in &rules.redirects {
            if let Some(caps) = r.pattern.captures(req.uri) {
                let target = r.target.as_ref().map(|t| {
                    if r.keeps_tail { format!("{t}{}", caps.get(1).map_or("", |m| m.as_str())) } else { substitute(t, &caps) }
                });
                decision.redirect = Some((r.status, target));
                break 'redirects;
            }
        }
    }

    let mut access: Option<&Access> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    for rules in chain {
        for section in &rules.sections {
            let applies = match &section.files {
                FileMatch::Any => true,
                FileMatch::Name(re) => re.is_match(req.file_name),
            };
            if !applies {
                continue;
            }
            // SetEnv and SetEnvIf run before Header, so a later file sees what an earlier one set.
            for (name, value) in &section.env.plain {
                set_env(&mut env, name, value);
            }
            for test in &section.env.tests {
                let matched = req.attribute(&test.attribute).is_some_and(|value| test.pattern.is_match(&value));
                if matched != test.negate {
                    set_env(&mut env, &test.set, &test.value);
                }
            }
            for name in &section.env.unset {
                env.retain(|(n, _)| n != name);
            }

            decision.headers.extend(section.headers.iter().cloned());
            if section.access.is_set() {
                access = Some(&section.access);
            }
            if let Some(indexes) = section.options_indexes {
                decision.indexes = Some(indexes);
            }
            for doc in &section.errordocs {
                decision.errordocs.retain(|d| d.code != doc.code);
                decision.errordocs.push(doc.clone());
            }
            decision.php.values.extend(section.php.values.iter().cloned());
            decision.php.flags.extend(section.php.flags.iter().cloned());
            let e = &section.expires;
            if e.active.is_some() {
                decision.expires.active = e.active;
            }
            if e.default.is_some() {
                decision.expires.default = e.default;
            }
            for (mime, secs) in &e.by_type {
                decision.expires.by_type.retain(|(m, _)| m != mime);
                decision.expires.by_type.push((mime.clone(), *secs));
            }
        }
    }
    // A Header env= condition is checked against the environment the same request built.
    let applies = |env: &Option<(String, bool)>, built: &[(String, String)]| match env {
        None => true,
        Some((name, negated)) => built.iter().any(|(n, _)| n == name) != *negated,
    };
    let built = std::mem::take(&mut env);
    let headers = std::mem::take(&mut decision.headers);
    decision.headers = headers.into_iter().filter(|h| applies(&h.env, &built)).collect();
    decision.env = built;
    decision.denied = access.is_some_and(|a| a.denies(req.client.as_ref()));
    decision
}

fn set_env(env: &mut Vec<(String, String)>, name: &str, value: &str) {
    match env.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value.to_string(),
        None => env.push((name.to_string(), value.to_string())),
    }
}

impl Expires {
    /// Seconds to cache a response of this content type, if expiry applies.
    pub fn seconds_for(&self, content_type: &str) -> Option<u64> {
        if self.active != Some(true) {
            return None;
        }
        let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        self.by_type
            .iter()
            .find(|(m, _)| *m == mime || (m.ends_with("/*") && mime.starts_with(&m[..m.len() - 1])))
            .map(|(_, s)| *s)
            .or(self.default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn rules(src: &str) -> Rules {
        Rules::compile(&parse(src).unwrap())
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    /// A request with no headers, for tests that only care about paths and rules.
    fn plain<'a>(uri: &'a str, file_name: &'a str, client: Option<IpAddr>) -> Request<'a> {
        Request { uri, file_name, method: "GET", client, header: &|_| None }
    }

    /// A request carrying one request header, for SetEnvIf tests.
    fn with_header<'a>(uri: &'a str, client: Option<IpAddr>, header: &'a dyn Fn(&str) -> Option<String>) -> Request<'a> {
        Request { uri, file_name: "", method: "GET", client, header }
    }

    /// The header closure `with_header` takes, kept alive by the caller.
    fn one_header<'a>(name: &'a str, value: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
        move |wanted: &str| wanted.eq_ignore_ascii_case(name).then(|| value.to_string())
    }

    #[test]
    fn headers_accumulate_from_parent_to_child_and_files_blocks_filter() {
        let parent = rules("Header always set X-Frame-Options \"SAMEORIGIN\"\nHeader unset X-Powered-By\n");
        let child = rules("<FilesMatch \"\\.css$\">\nHeader set Cache-Control \"max-age=60\"\n</FilesMatch>\n");
        let css = decide(&[&parent, &child], &plain("/a/b.css", "b.css", None));
        let names: Vec<_> = css.headers.iter().map(|h| (h.action, h.name.as_str())).collect();
        assert_eq!(names, [(HeaderAction::Set, "X-Frame-Options"), (HeaderAction::Unset, "X-Powered-By"), (HeaderAction::Set, "Cache-Control")]);
        assert_eq!(decide(&[&parent, &child], &plain("/a/b.js", "b.js", None)).headers.len(), 2);
    }

    #[test]
    fn unsafe_headers_and_conditions_are_skipped() {
        let r = rules("Header set Bad\\ Name x\nHeader edit X-B a b\nHeader set X-Ok \"1\"\n");
        let d = decide(&[&r], &plain("/", "", None));
        assert_eq!(d.headers.len(), 1);
        assert_eq!(r.notes.len(), 2);
    }

    #[test]
    fn negated_ifmodule_is_the_fallback_and_is_skipped() {
        let r = rules("<IfModule !mod_headers.c>\nHeader set X-Skip 1\n</IfModule>\n<IfModule mod_headers.c>\nHeader set X-Use 1\n</IfModule>\n");
        let d = decide(&[&r], &plain("/", "", None));
        assert_eq!(d.headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["X-Use"]);
    }

    #[test]
    fn redirect_keeps_the_tail_and_redirectmatch_substitutes() {
        let r = rules("Redirect 301 /old /new\nRedirectMatch ^/blog/(.*)$ /news/$1\nRedirect gone /dead\n");
        assert_eq!(decide(&[&r], &plain("/old/x/y", "", None)).redirect, Some((301, Some("/new/x/y".into()))));
        assert_eq!(decide(&[&r], &plain("/old", "", None)).redirect, Some((301, Some("/new".into()))));
        assert_eq!(decide(&[&r], &plain("/older", "", None)).redirect, None);
        assert_eq!(decide(&[&r], &plain("/blog/2026/post", "", None)).redirect, Some((302, Some("/news/2026/post".into()))));
        assert_eq!(decide(&[&r], &plain("/dead", "", None)).redirect, Some((410, None)));
    }

    #[test]
    fn redirect_targets_must_be_paths_or_http_urls() {
        let r = rules("Redirect /a javascript:alert(1)\nRedirect /b ftp://x\nRedirect /c https://example.com/c\n");
        assert_eq!(decide(&[&r], &plain("/a", "", None)).redirect, None);
        assert_eq!(decide(&[&r], &plain("/b", "", None)).redirect, None);
        assert!(decide(&[&r], &plain("/c", "", None)).redirect.is_some());
    }

    #[test]
    fn apache22_order_allow_deny() {
        let only_office = rules("Order Deny,Allow\nDeny from all\nAllow from 10.1.2.0/24 192.168\n");
        assert!(!decide(&[&only_office], &plain("/", "", ip("10.1.2.9"))).denied);
        assert!(!decide(&[&only_office], &plain("/", "", ip("192.168.5.5"))).denied);
        assert!(decide(&[&only_office], &plain("/", "", ip("8.8.8.8"))).denied);
        let block_one = rules("Order Allow,Deny\nAllow from all\nDeny from 1.2.3.4\n");
        assert!(decide(&[&block_one], &plain("/", "", ip("1.2.3.4"))).denied);
        assert!(!decide(&[&block_one], &plain("/", "", ip("1.2.3.5"))).denied);
    }

    #[test]
    fn apache24_require_and_files_deny() {
        let r = rules("<Files \".env\">\nRequire all denied\n</Files>\n<FilesMatch \"\\.(bak|sql)$\">\nRequire ip 10.0.0.0/8\n</FilesMatch>\n");
        assert!(decide(&[&r], &plain("/.env", ".env", ip("10.0.0.1"))).denied);
        assert!(!decide(&[&r], &plain("/index.php", "index.php", ip("1.1.1.1"))).denied);
        assert!(decide(&[&r], &plain("/db.sql", "db.sql", ip("1.1.1.1"))).denied);
        assert!(!decide(&[&r], &plain("/db.sql", "db.sql", ip("10.9.9.9"))).denied);
        let not = rules("Require not ip 5.5.5.5\n");
        assert!(decide(&[&not], &plain("/", "", ip("5.5.5.5"))).denied);
        assert!(!decide(&[&not], &plain("/", "", ip("5.5.5.6"))).denied);
    }

    #[test]
    fn child_access_replaces_parent_access() {
        let parent = rules("Require all denied\n");
        let child = rules("Require all granted\n");
        assert!(decide(&[&parent], &plain("/x", "", ip("1.1.1.1"))).denied);
        assert!(!decide(&[&parent, &child], &plain("/x/y", "", ip("1.1.1.1"))).denied);
    }

    #[test]
    fn ipv6_and_mapped_ipv4() {
        let r = rules("Require ip 2001:db8::/32 10.0.0.0/8\n");
        assert!(!decide(&[&r], &plain("/", "", ip("2001:db8::1"))).denied);
        assert!(!decide(&[&r], &plain("/", "", ip("::ffff:10.1.1.1"))).denied);
        assert!(decide(&[&r], &plain("/", "", ip("2001:db9::1"))).denied);
    }

    #[test]
    fn expires_by_type_default_and_child_override() {
        let parent = rules("ExpiresActive On\nExpiresByType image/png \"access plus 1 year\"\nExpiresByType text/* A60\nExpiresDefault \"access plus 1 month\"\n");
        let child = rules("ExpiresByType image/png \"access plus 1 day\"\n");
        let d = decide(&[&parent, &child], &plain("/a.png", "a.png", None));
        assert_eq!(d.expires.seconds_for("image/png"), Some(86_400));
        assert_eq!(d.expires.seconds_for("text/css; charset=utf-8"), Some(60));
        assert_eq!(d.expires.seconds_for("application/pdf"), Some(2_592_000));
        let off = rules("ExpiresActive Off\nExpiresDefault A60\n");
        assert_eq!(decide(&[&off], &plain("/", "", None)).expires.seconds_for("text/html"), None);
    }

    #[test]
    fn rewrite_rules_are_left_to_openlitespeed() {
        let r = rules("RewriteEngine On\nRewriteBase /\nRewriteRule ^index\\.php$ - [L]\n");
        assert!(r.notes.is_empty());
        let d = decide(&[&r], &plain("/", "", None));
        assert!(d.headers.is_empty() && d.redirect.is_none() && !d.denied);
    }

    #[test]
    fn setenv_and_header_env_conditions() {
        let r = rules("SetEnv NO_CACHE 1\nHeader set X-Cache \"off\" env=NO_CACHE\nHeader set X-Other \"y\" env=!NO_CACHE\n");
        let d = decide(&[&r], &plain("/", "", None));
        assert_eq!(d.env, [("NO_CACHE".to_string(), "1".to_string())]);
        assert_eq!(d.headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["X-Cache"]);

        // Without the variable the negated condition is the one that applies.
        let only_negated = rules("Header set X-Other \"y\" env=!NO_CACHE\n");
        let d = decide(&[&only_negated], &plain("/", "", None));
        assert_eq!(d.headers.len(), 1);
    }

    #[test]
    fn setenvif_matches_request_headers_and_unsetenv_removes_them() {
        let r = rules("SetEnvIf X-Forwarded-For \"^10\\.\" INTERNAL\nSetEnvIf Request_URI \"^/admin\" IS_ADMIN\n");
        let internal = {
            let h = one_header("X-Forwarded-For", "10.2.3.4");
            decide(&[&r], &with_header("/shop", ip("1.1.1.1"), &h))
        };
        assert!(internal.env.iter().any(|(n, _)| n == "INTERNAL"));
        assert!(!internal.env.iter().any(|(n, _)| n == "IS_ADMIN"));

        let admin = decide(&[&r], &plain("/admin/x", "", None));
        assert!(admin.env.iter().any(|(n, v)| n == "IS_ADMIN" && v == "1"));

        let cleared = rules("SetEnv KEEP 1\nUnsetEnv KEEP\n");
        assert!(decide(&[&cleared], &plain("/", "", None)).env.is_empty());
    }

    #[test]
    fn setenvifnocase_and_browsermatch() {
        let r = rules("SetEnvIfNoCase User-Agent \"wordpress\" WP\nBrowserMatch \"^curl/\" CLI\n");
        let d = {
            let h = one_header("User-Agent", "Mozilla/5.0 WordPress/6.5");
            decide(&[&r], &with_header("/", None, &h))
        };
        assert!(d.env.iter().any(|(n, _)| n == "WP"), "{:?} {:?}", d.env, r.notes);
        let curl = {
            let h = one_header("User-Agent", "curl/8.5.0");
            decide(&[&r], &with_header("/", None, &h))
        };
        assert!(curl.env.iter().any(|(n, _)| n == "CLI"));
    }

    #[test]
    fn negated_setenvif_sets_only_when_the_pattern_misses() {
        let r = rules("SetEnvIf User-Agent \"bot\" !HUMAN\n");
        assert!({
            let h = one_header("User-Agent", "bot");
            decide(&[&r], &with_header("/", None, &h))
        }.env.is_empty());
        let human = {
            let h = one_header("User-Agent", "Mozilla");
            decide(&[&r], &with_header("/", None, &h))
        };
        assert!(human.env.iter().any(|(n, _)| n == "HUMAN"));
    }

    #[test]
    fn php_values_and_flags_compile() {
        let r = rules("php_value memory_limit 768M\nphp_value upload_max_filesize 150M\nphp_flag display_errors Off\nphp_flag engine 0\n");
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        let d = decide(&[&r], &plain("/", "", None));
        assert_eq!(d.php.values, [("memory_limit".into(), "768M".into()), ("upload_max_filesize".into(), "150M".into())]);
        assert_eq!(d.php.flags, [("display_errors".into(), false), ("engine".into(), false)]);
    }

    #[test]
    fn errordocument_takes_paths_and_reports_urls_and_text() {
        let r = rules("ErrorDocument 404 /not-found.html\nErrorDocument 403 \"Access denied\"\nErrorDocument 500 https://example.com/oops\n");
        let d = decide(&[&r], &plain("/", "", None));
        assert_eq!(d.errordocs, [ErrorDocument { code: 404, target: "/not-found.html".into() }]);
        // A URL or a text body needs a response hatch cannot write, so both are reported.
        assert_eq!(r.notes.len(), 2);
        // The deepest file wins for the same code.
        let child = rules("ErrorDocument 404 /child-404.html\n");
        let merged = decide(&[&r, &child], &plain("/", "", None));
        assert_eq!(merged.errordocs.iter().filter(|d| d.code == 404).count(), 1);
        assert_eq!(merged.errordocs.iter().find(|d| d.code == 404).unwrap().target, "/child-404.html");
    }

    #[test]
    fn options_indexes_only_touches_listing() {
        let r = rules("Options -Indexes\n");
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert_eq!(decide(&[&r], &plain("/", "", None)).indexes, Some(false));
        let plus = rules("Options +Indexes\n");
        assert_eq!(decide(&[&plus], &plain("/", "", None)).indexes, Some(true));
        // Unrelated options are reported and do not change the listing setting.
        let mixed = rules("Options -Indexes -FollowSymLinks\n");
        assert_eq!(mixed.notes.len(), 1);
        assert_eq!(decide(&[&mixed], &plain("/", "", None)).indexes, Some(false));
    }
}
