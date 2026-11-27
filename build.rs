fn main() {
    let src = std::path::Path::new("grammar/src");
    cc::Build::new()
        .include(src)
        .file(src.join("parser.c"))
        .file(src.join("scanner.c"))
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .compile("tree-sitter-rpmspec");
    println!("cargo:rerun-if-changed=grammar/src");
}
