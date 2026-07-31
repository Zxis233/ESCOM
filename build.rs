include!("icon_pixels.rs");

fn main() {
    println!("cargo:rerun-if-changed=icon_pixels.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let output_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let icon_path = output_dir.join("escom.ico");
    std::fs::write(&icon_path, encode_ico(&escom_icon_rgba())).expect("write generated icon");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon_path.to_str().expect("UTF-8 output path"));
    resource.compile().expect("compile Windows resources");
}

fn encode_ico(rgba: &[u8]) -> Vec<u8> {
    let width = ICON_SIZE;
    let height = ICON_SIZE;
    let xor_size = width * height * 4;
    let mask_stride = width.div_ceil(32) * 4;
    let mask_size = mask_stride * height;
    let bitmap_size = 40 + xor_size + mask_size;
    let mut output = Vec::with_capacity((22 + bitmap_size) as usize);

    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.push(width as u8);
    output.push(height as u8);
    output.push(0);
    output.push(0);
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&32_u16.to_le_bytes());
    output.extend_from_slice(&bitmap_size.to_le_bytes());
    output.extend_from_slice(&22_u32.to_le_bytes());

    output.extend_from_slice(&40_u32.to_le_bytes());
    output.extend_from_slice(&(width as i32).to_le_bytes());
    output.extend_from_slice(&((height * 2) as i32).to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&32_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&xor_size.to_le_bytes());
    output.extend_from_slice(&0_i32.to_le_bytes());
    output.extend_from_slice(&0_i32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());

    for y in (0..height).rev() {
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            output.extend_from_slice(&[
                rgba[index + 2],
                rgba[index + 1],
                rgba[index],
                rgba[index + 3],
            ]);
        }
    }
    output.resize(output.len() + mask_size as usize, 0);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ico_has_expected_header_and_size() {
        let icon = encode_ico(&escom_icon_rgba());
        assert_eq!(&icon[..6], &[0, 0, 1, 0, 1, 0]);
        assert!(icon.len() > (ICON_SIZE * ICON_SIZE * 4) as usize);
    }
}
