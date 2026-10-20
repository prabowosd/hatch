fn main() {
    println!("cargo:rerun-if-changed=ols-api/ls.h");
    println!("cargo:rerun-if-changed=src/shim.c");
    cc::Build::new().file("src/shim.c").include("ols-api").compile("hatchshim");
    let bindings = bindgen::Builder::default()
        .header("ols-api/ls.h")
        .clang_arg("-Iols-api")
        .allowlist_type("lsi_.*|LSI_.*|evtcbhead_s|ls_str_s")
        .allowlist_item("lsi_.*|LSI_.*")
        .allowlist_var("LSI_.*|LSIAPI_.*")
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("generate LSIAPI bindings");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("lsiapi.rs")).expect("write bindings");
}
