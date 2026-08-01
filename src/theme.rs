use eframe::egui::{self, Color32};

const MAX_SAMPLE_SIDE: usize = 64;
const HUE_BUCKET_COUNT: usize = 24;
const MIN_ALPHA: u8 = 64;
const MIN_SATURATION: f32 = 0.12;
const MIN_VALUE: f32 = 0.06;
const MIN_TEXT_CONTRAST: f32 = 4.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicAccent {
    pub source: Color32,
    pub light: Color32,
    pub dark: Color32,
}

#[derive(Default)]
pub struct DynamicTheme {
    analyzed_uri: Option<String>,
    accent: Option<DynamicAccent>,
    applied: bool,
}

impl DynamicTheme {
    /// Analyze and apply a background image once for each URI.
    pub fn update_from_image(
        &mut self,
        context: &egui::Context,
        uri: &str,
        image: &egui::ColorImage,
    ) {
        if self.analyzed_uri.as_deref() == Some(uri) {
            return;
        }

        let accent = extract_dynamic_accent(image);
        self.analyzed_uri = Some(uri.to_owned());
        self.accent = accent;

        if let Some(accent) = accent {
            apply_dynamic_accent(context, accent);
            self.applied = true;
        } else if self.applied {
            restore_default_accent(context);
            self.applied = false;
        }
        context.request_repaint();
    }

    /// Forget the analyzed image and restore egui's standard accent colors.
    pub fn clear(&mut self, context: &egui::Context) {
        let should_repaint = self.analyzed_uri.take().is_some() || self.applied;
        self.accent = None;
        if self.applied {
            restore_default_accent(context);
            self.applied = false;
        }
        if should_repaint {
            context.request_repaint();
        }
    }

    pub fn accent(&self) -> Option<DynamicAccent> {
        self.accent
    }
}

#[derive(Clone, Copy, Default)]
struct HueBucket {
    score: f32,
    red: f32,
    green: f32,
    blue: f32,
    weight: f32,
    count: usize,
}

#[derive(Clone, Copy)]
struct Hsv {
    hue: f32,
    saturation: f32,
    value: f32,
}

fn extract_dynamic_accent(image: &egui::ColorImage) -> Option<DynamicAccent> {
    let [width, height] = image.size;
    let pixel_count = width.checked_mul(height)?;
    if pixel_count == 0 || image.pixels.len() < pixel_count {
        return None;
    }

    let step_x = width.div_ceil(MAX_SAMPLE_SIDE).max(1);
    let step_y = height.div_ceil(MAX_SAMPLE_SIDE).max(1);
    let mut buckets = [HueBucket::default(); HUE_BUCKET_COUNT];
    let mut sampled_count = 0usize;

    for y in (0..height).step_by(step_y) {
        for x in (0..width).step_by(step_x) {
            let [red, green, blue, alpha] = image.pixels[y * width + x].to_srgba_unmultiplied();
            if alpha < MIN_ALPHA {
                continue;
            }
            sampled_count += 1;

            let red = f32::from(red) / 255.0;
            let green = f32::from(green) / 255.0;
            let blue = f32::from(blue) / 255.0;
            let hsv = rgb_to_hsv(red, green, blue);
            if hsv.saturation < MIN_SATURATION || hsv.value < MIN_VALUE {
                continue;
            }

            let alpha_weight = f32::from(alpha) / 255.0;
            let middle_value = 1.0 - ((hsv.value - 0.5).abs() * 2.0).clamp(0.0, 1.0);
            let color_weight = 0.15 + 0.85 * hsv.saturation * hsv.saturation;
            let weight = alpha_weight * color_weight * (0.75 + 0.25 * middle_value);
            let bucket_index =
                ((hsv.hue * HUE_BUCKET_COUNT as f32) as usize).min(HUE_BUCKET_COUNT - 1);
            let bucket = &mut buckets[bucket_index];
            bucket.score += weight;
            bucket.red += red * weight;
            bucket.green += green * weight;
            bucket.blue += blue * weight;
            bucket.weight += weight;
            bucket.count += 1;
        }
    }

    if sampled_count == 0 {
        return None;
    }

    // Include neighboring hue buckets when ranking and averaging. This avoids
    // splitting colors such as red across the 0°/360° boundary.
    let dominant_index = (0..HUE_BUCKET_COUNT).max_by(|left, right| {
        neighborhood_score(&buckets, *left).total_cmp(&neighborhood_score(&buckets, *right))
    })?;
    let neighbors = [
        (dominant_index + HUE_BUCKET_COUNT - 1) % HUE_BUCKET_COUNT,
        dominant_index,
        (dominant_index + 1) % HUE_BUCKET_COUNT,
    ];
    let mut combined = HueBucket::default();
    for index in neighbors {
        let bucket = buckets[index];
        combined.red += bucket.red;
        combined.green += bucket.green;
        combined.blue += bucket.blue;
        combined.weight += bucket.weight;
        combined.count += bucket.count;
    }

    let minimum_samples = if sampled_count < 16 {
        1
    } else {
        (sampled_count / 256).max(2)
    };
    if combined.count < minimum_samples || combined.weight <= f32::EPSILON {
        return None;
    }

    let red = (combined.red / combined.weight).clamp(0.0, 1.0);
    let green = (combined.green / combined.weight).clamp(0.0, 1.0);
    let blue = (combined.blue / combined.weight).clamp(0.0, 1.0);
    let representative = rgb_to_hsv(red, green, blue);
    if representative.saturation < MIN_SATURATION {
        return None;
    }

    let source = rgb_to_color32(red, green, blue);
    let light = accessible_accent(
        representative.hue,
        (representative.saturation * 0.62).clamp(0.35, 0.58),
        0.94,
        false,
    );
    let dark = accessible_accent(
        representative.hue,
        (representative.saturation * 1.05).clamp(0.62, 0.88),
        0.52,
        true,
    );

    Some(DynamicAccent {
        source,
        light,
        dark,
    })
}

fn neighborhood_score(buckets: &[HueBucket; HUE_BUCKET_COUNT], index: usize) -> f32 {
    let previous = (index + HUE_BUCKET_COUNT - 1) % HUE_BUCKET_COUNT;
    let next = (index + 1) % HUE_BUCKET_COUNT;
    buckets[index].score + 0.45 * (buckets[previous].score + buckets[next].score)
}

fn accessible_accent(hue: f32, saturation: f32, mut value: f32, dark_mode: bool) -> Color32 {
    let text_color = if dark_mode {
        Color32::from_gray(245)
    } else {
        Color32::from_gray(18)
    };
    for _ in 0..32 {
        let color = hsv_to_color32(Hsv {
            hue,
            saturation,
            value,
        });
        if contrast_ratio(color, text_color) >= MIN_TEXT_CONTRAST {
            return color;
        }
        value = if dark_mode {
            (value - 0.025).max(0.18)
        } else {
            (value + 0.025).min(1.0)
        };
    }
    hsv_to_color32(Hsv {
        hue,
        saturation,
        value,
    })
}

fn apply_dynamic_accent(context: &egui::Context, accent: DynamicAccent) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        context.style_mut_of(theme, |style| {
            let dark_mode = style.visuals.dark_mode;
            let color = if dark_mode { accent.dark } else { accent.light };
            apply_accent_to_visuals(&mut style.visuals, color, dark_mode);
        });
    }
}

fn restore_default_accent(context: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        context.style_mut_of(theme, |style| {
            restore_default_accent_for_visuals(&mut style.visuals);
        });
    }
}

fn apply_accent_to_visuals(visuals: &mut egui::Visuals, accent: Color32, dark_mode: bool) {
    restore_default_accent_for_visuals(visuals);
    let text_color = if dark_mode {
        Color32::from_gray(245)
    } else {
        Color32::from_gray(18)
    };
    let default_visuals = default_visuals(dark_mode);

    visuals.selection.bg_fill = accent;
    visuals.selection.stroke.color = text_color;

    let hovered_fill = default_visuals
        .widgets
        .hovered
        .bg_fill
        .lerp_to_gamma(accent, 0.32);
    let hovered_weak_fill = default_visuals
        .widgets
        .hovered
        .weak_bg_fill
        .lerp_to_gamma(accent, 0.32);
    visuals.widgets.hovered.bg_fill = hovered_fill;
    visuals.widgets.hovered.weak_bg_fill = hovered_weak_fill;
    visuals.widgets.hovered.bg_stroke.color = default_visuals
        .widgets
        .hovered
        .bg_stroke
        .color
        .lerp_to_gamma(accent, 0.72);

    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.active.weak_bg_fill = accent;
    visuals.widgets.active.bg_stroke.color = text_color;
    visuals.widgets.active.fg_stroke.color = text_color;

    visuals.widgets.open.bg_fill = default_visuals
        .widgets
        .open
        .bg_fill
        .lerp_to_gamma(accent, 0.28);
    visuals.widgets.open.weak_bg_fill = default_visuals
        .widgets
        .open
        .weak_bg_fill
        .lerp_to_gamma(accent, 0.28);
    visuals.widgets.open.bg_stroke.color = default_visuals
        .widgets
        .open
        .bg_stroke
        .color
        .lerp_to_gamma(accent, 0.68);
}

fn restore_default_accent_for_visuals(visuals: &mut egui::Visuals) {
    let defaults = default_visuals(visuals.dark_mode);
    visuals.selection = defaults.selection;

    visuals.widgets.hovered.bg_fill = defaults.widgets.hovered.bg_fill;
    visuals.widgets.hovered.weak_bg_fill = defaults.widgets.hovered.weak_bg_fill;
    visuals.widgets.hovered.bg_stroke.color = defaults.widgets.hovered.bg_stroke.color;

    visuals.widgets.active.bg_fill = defaults.widgets.active.bg_fill;
    visuals.widgets.active.weak_bg_fill = defaults.widgets.active.weak_bg_fill;
    visuals.widgets.active.bg_stroke.color = defaults.widgets.active.bg_stroke.color;
    visuals.widgets.active.fg_stroke.color = defaults.widgets.active.fg_stroke.color;

    visuals.widgets.open.bg_fill = defaults.widgets.open.bg_fill;
    visuals.widgets.open.weak_bg_fill = defaults.widgets.open.weak_bg_fill;
    visuals.widgets.open.bg_stroke.color = defaults.widgets.open.bg_stroke.color;
}

fn default_visuals(dark_mode: bool) -> egui::Visuals {
    if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    }
}

fn rgb_to_hsv(red: f32, green: f32, blue: f32) -> Hsv {
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let chroma = maximum - minimum;
    let hue = if chroma <= f32::EPSILON {
        0.0
    } else if maximum == red {
        ((green - blue) / chroma).rem_euclid(6.0) / 6.0
    } else if maximum == green {
        ((blue - red) / chroma + 2.0) / 6.0
    } else {
        ((red - green) / chroma + 4.0) / 6.0
    };
    let saturation = if maximum <= f32::EPSILON {
        0.0
    } else {
        chroma / maximum
    };
    Hsv {
        hue,
        saturation,
        value: maximum,
    }
}

fn hsv_to_color32(hsv: Hsv) -> Color32 {
    let scaled_hue = hsv.hue.rem_euclid(1.0) * 6.0;
    let chroma = hsv.value * hsv.saturation;
    let secondary = chroma * (1.0 - (scaled_hue.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match scaled_hue as u8 {
        0 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let match_value = hsv.value - chroma;
    rgb_to_color32(red + match_value, green + match_value, blue + match_value)
}

fn rgb_to_color32(red: f32, green: f32, blue: f32) -> Color32 {
    Color32::from_rgb(
        (red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (blue.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn contrast_ratio(left: Color32, right: Color32) -> f32 {
    let left_luminance = relative_luminance(left);
    let right_luminance = relative_luminance(right);
    let lighter = left_luminance.max(right_luminance);
    let darker = left_luminance.min(right_luminance);
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: Color32) -> f32 {
    fn linear_channel(channel: u8) -> f32 {
        let channel = f32::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * linear_channel(color.r())
        + 0.7152 * linear_channel(color.g())
        + 0.0722 * linear_channel(color.b())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominant_chromatic_region_is_selected() {
        let mut pixels = vec![Color32::from_rgb(30, 180, 80); 80];
        pixels.extend(vec![Color32::from_rgb(220, 40, 40); 20]);
        let image = egui::ColorImage::new([10, 10], pixels);

        let accent = extract_dynamic_accent(&image).expect("accent should be extracted");
        let hsv = rgb_to_hsv(
            f32::from(accent.source.r()) / 255.0,
            f32::from(accent.source.g()) / 255.0,
            f32::from(accent.source.b()) / 255.0,
        );

        assert!((0.25..=0.45).contains(&hsv.hue));
    }

    #[test]
    fn grayscale_image_uses_default_theme() {
        let mut pixels = vec![Color32::BLACK; 50];
        pixels.extend(vec![Color32::WHITE; 50]);
        let image = egui::ColorImage::new([10, 10], pixels);

        assert!(extract_dynamic_accent(&image).is_none());
    }

    #[test]
    fn transparent_pixels_do_not_influence_the_accent() {
        let mut pixels = vec![Color32::from_rgb(20, 80, 220); 64];
        pixels.extend(vec![Color32::from_rgba_unmultiplied(255, 0, 0, 0); 64]);
        let image = egui::ColorImage::new([16, 8], pixels);

        let accent = extract_dynamic_accent(&image).expect("accent should be extracted");
        let hsv = rgb_to_hsv(
            f32::from(accent.source.r()) / 255.0,
            f32::from(accent.source.g()) / 255.0,
            f32::from(accent.source.b()) / 255.0,
        );
        assert!((0.55..=0.72).contains(&hsv.hue));
    }

    #[test]
    fn generated_colors_keep_text_readable() {
        for source in [
            Color32::from_rgb(255, 230, 0),
            Color32::from_rgb(0, 70, 255),
            Color32::from_rgb(230, 25, 80),
        ] {
            let image = egui::ColorImage::filled([16, 16], source);
            let accent = extract_dynamic_accent(&image).expect("accent should be extracted");
            assert!(contrast_ratio(accent.light, Color32::from_gray(18)) >= MIN_TEXT_CONTRAST);
            assert!(contrast_ratio(accent.dark, Color32::from_gray(245)) >= MIN_TEXT_CONTRAST);
        }
    }

    #[test]
    fn applying_and_restoring_only_changes_accent_visuals() {
        let mut visuals = egui::Visuals::dark();
        let defaults = visuals.clone();

        apply_accent_to_visuals(&mut visuals, Color32::from_rgb(120, 35, 65), true);
        assert_ne!(visuals.selection.bg_fill, defaults.selection.bg_fill);
        assert_ne!(
            visuals.widgets.active.weak_bg_fill,
            defaults.widgets.active.weak_bg_fill
        );
        assert_eq!(visuals.panel_fill, defaults.panel_fill);

        restore_default_accent_for_visuals(&mut visuals);
        assert_eq!(visuals.selection, defaults.selection);
        assert_eq!(visuals.widgets.active, defaults.widgets.active);
        assert_eq!(visuals.widgets.hovered, defaults.widgets.hovered);
        assert_eq!(visuals.widgets.open, defaults.widgets.open);
    }
}
