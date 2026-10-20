//! hatch: Apache .htaccess compatibility for OpenLiteSpeed.
pub mod parse;
pub mod rules;
pub mod module;

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/lsiapi.rs"));
}
