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
        if args.len() > extra {
            return self.note(line, "Header conditions (env=, expr=) are not supported yet, ignored");
        }
        if !header_name_ok(name) || !value_ok(&value) || (action != HeaderAction::Unset && value.is_empty()) {
            return self.note(line, "Header has an invalid name or value, ignored");
        }
        self.rules.sections[s].headers.push(HeaderOp { action, name: name.clone(), value });
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
        self.rules.redirects.push(Redirect { pattern, target, status, keeps_tail: !source.is_empty() });
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
        let root = Section { files: FileMatch::Any, headers: Vec::new(), access: Access::default(), expires: Expires::default() };
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

/// Evaluate the .htaccess chain for a request. `chain` runs from the docroot down to the file's folder.
pub fn decide(chain: &[&Rules], uri: &str, file_name: &str, client: Option<IpAddr>) -> Decision {
    let mut decision = Decision::default();

    // The deepest folder's redirects win, as with mod_alias in per-directory context.
    'redirects: for rules in chain.iter().rev() {
        for r in &rules.redirects {
            if let Some(caps) = r.pattern.captures(uri) {
                let target = r.target.as_ref().map(|t| {
                    if r.keeps_tail { format!("{t}{}", caps.get(1).map_or("", |m| m.as_str())) } else { substitute(t, &caps) }
                });
                decision.redirect = Some((r.status, target));
                break 'redirects;
            }
        }
    }

    let mut access: Option<&Access> = None;
    for rules in chain {
        for section in &rules.sections {
            let applies = match &section.files {
                FileMatch::Any => true,
                FileMatch::Name(re) => re.is_match(file_name),
            };
            if !applies {
                continue;
            }
            decision.headers.extend(section.headers.iter().cloned());
            if section.access.is_set() {
                access = Some(&section.access);
            }
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
    decision.denied = access.is_some_and(|a| a.denies(client.as_ref()));
    decision
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

    #[test]
    fn headers_accumulate_from_parent_to_child_and_files_blocks_filter() {
        let parent = rules("Header always set X-Frame-Options \"SAMEORIGIN\"\nHeader unset X-Powered-By\n");
        let child = rules("<FilesMatch \"\\.css$\">\nHeader set Cache-Control \"max-age=60\"\n</FilesMatch>\n");
        let css = decide(&[&parent, &child], "/a/b.css", "b.css", None);
        let names: Vec<_> = css.headers.iter().map(|h| (h.action, h.name.as_str())).collect();
        assert_eq!(names, [(HeaderAction::Set, "X-Frame-Options"), (HeaderAction::Unset, "X-Powered-By"), (HeaderAction::Set, "Cache-Control")]);
        assert_eq!(decide(&[&parent, &child], "/a/b.js", "b.js", None).headers.len(), 2);
    }

    #[test]
    fn unsafe_headers_and_conditions_are_skipped() {
        let r = rules("Header set Bad\\ Name x\nHeader set X-A \"v\" env=foo\nHeader edit X-B a b\nHeader set X-Ok \"1\"\n");
        let d = decide(&[&r], "/", "", None);
        assert_eq!(d.headers.len(), 1);
        assert_eq!(r.notes.len(), 3);
    }

    #[test]
    fn negated_ifmodule_is_the_fallback_and_is_skipped() {
        let r = rules("<IfModule !mod_headers.c>\nHeader set X-Skip 1\n</IfModule>\n<IfModule mod_headers.c>\nHeader set X-Use 1\n</IfModule>\n");
        let d = decide(&[&r], "/", "", None);
        assert_eq!(d.headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["X-Use"]);
    }

    #[test]
    fn redirect_keeps_the_tail_and_redirectmatch_substitutes() {
        let r = rules("Redirect 301 /old /new\nRedirectMatch ^/blog/(.*)$ /news/$1\nRedirect gone /dead\n");
        assert_eq!(decide(&[&r], "/old/x/y", "", None).redirect, Some((301, Some("/new/x/y".into()))));
        assert_eq!(decide(&[&r], "/old", "", None).redirect, Some((301, Some("/new".into()))));
        assert_eq!(decide(&[&r], "/older", "", None).redirect, None);
        assert_eq!(decide(&[&r], "/blog/2026/post", "", None).redirect, Some((302, Some("/news/2026/post".into()))));
        assert_eq!(decide(&[&r], "/dead", "", None).redirect, Some((410, None)));
    }

    #[test]
    fn redirect_targets_must_be_paths_or_http_urls() {
        let r = rules("Redirect /a javascript:alert(1)\nRedirect /b ftp://x\nRedirect /c https://example.com/c\n");
        assert_eq!(decide(&[&r], "/a", "", None).redirect, None);
        assert_eq!(decide(&[&r], "/b", "", None).redirect, None);
        assert!(decide(&[&r], "/c", "", None).redirect.is_some());
    }

    #[test]
    fn apache22_order_allow_deny() {
        let only_office = rules("Order Deny,Allow\nDeny from all\nAllow from 10.1.2.0/24 192.168\n");
        assert!(!decide(&[&only_office], "/", "", ip("10.1.2.9")).denied);
        assert!(!decide(&[&only_office], "/", "", ip("192.168.5.5")).denied);
        assert!(decide(&[&only_office], "/", "", ip("8.8.8.8")).denied);
        let block_one = rules("Order Allow,Deny\nAllow from all\nDeny from 1.2.3.4\n");
        assert!(decide(&[&block_one], "/", "", ip("1.2.3.4")).denied);
        assert!(!decide(&[&block_one], "/", "", ip("1.2.3.5")).denied);
    }

    #[test]
    fn apache24_require_and_files_deny() {
        let r = rules("<Files \".env\">\nRequire all denied\n</Files>\n<FilesMatch \"\\.(bak|sql)$\">\nRequire ip 10.0.0.0/8\n</FilesMatch>\n");
        assert!(decide(&[&r], "/.env", ".env", ip("10.0.0.1")).denied);
        assert!(!decide(&[&r], "/index.php", "index.php", ip("1.1.1.1")).denied);
        assert!(decide(&[&r], "/db.sql", "db.sql", ip("1.1.1.1")).denied);
        assert!(!decide(&[&r], "/db.sql", "db.sql", ip("10.9.9.9")).denied);
        let not = rules("Require not ip 5.5.5.5\n");
        assert!(decide(&[&not], "/", "", ip("5.5.5.5")).denied);
        assert!(!decide(&[&not], "/", "", ip("5.5.5.6")).denied);
    }

    #[test]
    fn child_access_replaces_parent_access() {
        let parent = rules("Require all denied\n");
        let child = rules("Require all granted\n");
        assert!(decide(&[&parent], "/x", "", ip("1.1.1.1")).denied);
        assert!(!decide(&[&parent, &child], "/x/y", "", ip("1.1.1.1")).denied);
    }

    #[test]
    fn ipv6_and_mapped_ipv4() {
        let r = rules("Require ip 2001:db8::/32 10.0.0.0/8\n");
        assert!(!decide(&[&r], "/", "", ip("2001:db8::1")).denied);
        assert!(!decide(&[&r], "/", "", ip("::ffff:10.1.1.1")).denied);
        assert!(decide(&[&r], "/", "", ip("2001:db9::1")).denied);
    }

    #[test]
    fn expires_by_type_default_and_child_override() {
        let parent = rules("ExpiresActive On\nExpiresByType image/png \"access plus 1 year\"\nExpiresByType text/* A60\nExpiresDefault \"access plus 1 month\"\n");
        let child = rules("ExpiresByType image/png \"access plus 1 day\"\n");
        let d = decide(&[&parent, &child], "/a.png", "a.png", None);
        assert_eq!(d.expires.seconds_for("image/png"), Some(86_400));
        assert_eq!(d.expires.seconds_for("text/css; charset=utf-8"), Some(60));
        assert_eq!(d.expires.seconds_for("application/pdf"), Some(2_592_000));
        let off = rules("ExpiresActive Off\nExpiresDefault A60\n");
        assert_eq!(decide(&[&off], "/", "", None).expires.seconds_for("text/html"), None);
    }

    #[test]
    fn rewrite_rules_are_left_to_openlitespeed() {
        let r = rules("RewriteEngine On\nRewriteBase /\nRewriteRule ^index\\.php$ - [L]\n");
        assert!(r.notes.is_empty());
        let d = decide(&[&r], "/", "", None);
        assert!(d.headers.is_empty() && d.redirect.is_none() && !d.denied);
    }
}
