fn main() {
    // `-undefined dynamic_lookup` on macOS; maturin sets PYO3_BUILD_EXTENSION_MODULE.
    pyo3_build_config::add_extension_module_link_args();
}
