include!("../../windows_icon.rs");

fn main() {
    println!("cargo:rerun-if-changed=../../windows_icon.rs");
    println!("cargo:rerun-if-changed=../../icon_pixels.rs");
    compile_windows_icon();
}
