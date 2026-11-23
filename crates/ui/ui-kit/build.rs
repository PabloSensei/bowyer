//! The icons and fonts are embedded when this crate compiles. Cargo sees a changed file
//! that is embedded, but not a new one: watching the folder rebuilds the crate when an
//! asset is added or removed.

fn main() {
    println!("cargo:rerun-if-changed=assets");
}
