fn main() {
    // SQLx embeds existing SQL files; track the unified storage crate directory
    // as well so adding a migration invalidates a cached executable on stable Rust.
    println!("cargo:rerun-if-changed=crates/ohc-storage/migrations");
}
