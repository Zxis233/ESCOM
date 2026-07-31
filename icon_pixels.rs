pub const ICON_SIZE: u32 = 64;

pub fn escom_icon_rgba() -> Vec<u8> {
    let mut pixels = vec![0_u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let corner_x = if x < 10 {
                10 - x
            } else if x >= ICON_SIZE - 10 {
                x - (ICON_SIZE - 11)
            } else {
                0
            };
            let corner_y = if y < 10 {
                10 - y
            } else if y >= ICON_SIZE - 10 {
                y - (ICON_SIZE - 11)
            } else {
                0
            };
            if corner_x * corner_x + corner_y * corner_y <= 100 {
                set_pixel(&mut pixels, x, y, [31, 41, 51, 255]);
            }
        }
    }

    fill_rect(&mut pixels, 10, 20, 27, 44, [242, 246, 248, 255]);
    fill_rect(&mut pixels, 37, 20, 54, 44, [242, 246, 248, 255]);
    fill_rect(&mut pixels, 27, 29, 37, 35, [21, 184, 166, 255]);
    fill_rect(&mut pixels, 14, 24, 23, 40, [31, 41, 51, 255]);
    fill_rect(&mut pixels, 41, 24, 50, 40, [31, 41, 51, 255]);

    for y in [26, 32, 38] {
        fill_rect(&mut pixels, 16, y - 1, 21, y + 1, [75, 198, 225, 255]);
        fill_rect(&mut pixels, 43, y - 1, 48, y + 1, [75, 198, 225, 255]);
    }

    pixels
}

fn fill_rect(pixels: &mut [u8], left: u32, top: u32, right: u32, bottom: u32, color: [u8; 4]) {
    for y in top..bottom {
        for x in left..right {
            set_pixel(pixels, x, y, color);
        }
    }
}

fn set_pixel(pixels: &mut [u8], x: u32, y: u32, color: [u8; 4]) {
    let index = ((y * ICON_SIZE + x) * 4) as usize;
    pixels[index..index + 4].copy_from_slice(&color);
}

