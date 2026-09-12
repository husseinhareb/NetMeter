fn main() {
    // Only the GUI binary needs Tauri's codegen. `netmeterd` links this crate
    // with `--no-default-features`, where running it would be wasted work at
    // best and a hard failure at worst.
    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        tauri_build::build()
    }
}
