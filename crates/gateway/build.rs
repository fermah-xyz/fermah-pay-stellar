// `sqlx::migrate!` embeds the migration files at compile time but can only
// ask the compiler to watch files that already exist; watching the directory
// makes an added migration rebuild this crate instead of silently shipping
// the previous migration set.
fn main() {
    println!("cargo:rerun-if-changed=../../db/migrations");
}
