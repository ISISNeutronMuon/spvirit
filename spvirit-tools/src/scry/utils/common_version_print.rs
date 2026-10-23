// function for printing the version of the program based in vars set by vergen

pub fn version_header() {
    println!(
        // The name was env!("CARGO_PKG_NAME") before the move into spvirit-tools.
        "spvirit-scry v{} ({}) [{}]",
        env!("VERGEN_GIT_DESCRIBE"),
        env!("VERGEN_GIT_BRANCH"),
        env!("VERGEN_GIT_SHA")
    );
    println!("Built: {}", env!("VERGEN_GIT_COMMIT_TIMESTAMP"));
}
