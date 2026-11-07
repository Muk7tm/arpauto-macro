fn main() {
    // All widgets are custom (ui/widgets.slint); the style only affects Slint internals
    // such as text-selection handles, so pick the dark variant to match the palette.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint UI failed to compile");
}
