//! Rebuild when the migrations change. `sqlx::migrate!` embeds
//! `../../migrations` at compile time and tracks only the files it found, so
//! a new migration left the embedded schema stale until `cargo clean`.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
