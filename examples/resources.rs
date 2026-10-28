//! Lists a PE file's resources of the common types:  resources <exe or dll>
use rustschweine::image::{Image, ResId, list_resources};
use std::path::Path;
fn main() {
    let img = Image::load(Path::new(&std::env::args().nth(1).expect("path to an EXE or DLL"))).unwrap();
    let rd = |a: u32| img.byte(a);
    for (ty, name) in [(1, "cursor"), (2, "bitmap"), (3, "icon"), (4, "menu"), (5, "dialog"), (6, "string"), (9, "accelerators"), (10, "rcdata"), (14, "group icon"), (16, "version")] {
        for (n, a, size) in list_resources(&rd, img.base, ResId::Id(ty)) {
            println!("{name:<12} {n:?} at {a:#x}, {size} bytes");
        }
    }
}
