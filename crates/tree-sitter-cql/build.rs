fn main() {
    let mut build = cc::Build::new();
    build.include("src");
    // Warnings from generated parser code are noise.
    build.warnings(false);
    for src in ["src/parser.c", "src/scanner.c"] {
        if std::path::Path::new(src).exists() {
            build.file(src);
        }
    }
    build.compile("tree-sitter-cql");
    println!("cargo:rerun-if-changed=src/parser.c");
    println!("cargo:rerun-if-changed=src/scanner.c");
}
