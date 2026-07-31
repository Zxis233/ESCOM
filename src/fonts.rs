use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eframe::egui::epaint::text::VariationCoords;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId, TextStyle};
use fontdb::{Database, ID, Style};

const UI_FONT_KEY: &str = "escom_ui_font";
const DATA_FONT_KEY: &str = "escom_data_font";
const DATA_FAMILY_KEY: &str = "escom_data_family";
pub const DEFAULT_FONT_WEIGHT: u16 = 400;
const STANDARD_FONT_WEIGHTS: [u16; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];

#[derive(Clone, Copy, Debug)]
struct FontFace {
    id: ID,
    weight: u16,
    style: Style,
}

#[derive(Clone, Debug)]
enum WeightProfile {
    Variable {
        face: FontFace,
        min: f32,
        max: f32,
        default: u16,
    },
    Static {
        faces: Vec<FontFace>,
        default: u16,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontWeightOption {
    pub value: u16,
    pub label: String,
}

pub struct FontCatalog {
    database: Database,
    faces: BTreeMap<String, Vec<FontFace>>,
    weight_profiles: RefCell<BTreeMap<String, Option<WeightProfile>>>,
    ui_families: Vec<String>,
    mono_families: Vec<String>,
}

pub struct AppliedFonts {
    pub ui_family: String,
    pub data_family: String,
    pub ui_weight: u16,
    pub data_weight: u16,
    pub warnings: Vec<String>,
}

impl FontCatalog {
    pub fn load() -> Self {
        let mut database = Database::new();
        database.load_system_fonts();

        let mut faces: BTreeMap<String, Vec<FontFace>> = BTreeMap::new();
        let mut mono_names = BTreeSet::new();
        for face in database.faces() {
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            faces.entry(family.clone()).or_default().push(FontFace {
                id: face.id,
                weight: face.weight.0,
                style: face.style,
            });
            if face.monospaced {
                mono_names.insert(family.clone());
            }
        }

        // Weight selection is upright-only. Keep another style only for unusual
        // families which do not provide any upright face at all.
        for family_faces in faces.values_mut() {
            if family_faces.iter().any(|face| face.style == Style::Normal) {
                family_faces.retain(|face| face.style == Style::Normal);
            }
            family_faces.sort_by_key(|face| face.weight);
        }

        let ui_families = faces.keys().cloned().collect();
        let mono_families = mono_names
            .into_iter()
            .filter(|name| faces.contains_key(name))
            .collect();

        Self {
            database,
            faces,
            weight_profiles: RefCell::new(BTreeMap::new()),
            ui_families,
            mono_families,
        }
    }

    pub fn ui_families(&self) -> &[String] {
        &self.ui_families
    }

    pub fn mono_families(&self) -> &[String] {
        &self.mono_families
    }

    pub fn weight_options(&self, family: &str) -> Vec<FontWeightOption> {
        let Some(profile) = self.weight_profile(family) else {
            return vec![font_weight_option(DEFAULT_FONT_WEIGHT, DEFAULT_FONT_WEIGHT)];
        };
        let (weights, default) = match profile {
            WeightProfile::Variable {
                min, max, default, ..
            } => (variable_weight_options(min, max, default), default),
            WeightProfile::Static { faces, default } => {
                (faces.into_iter().map(|face| face.weight).collect(), default)
            }
        };

        let mut weights: Vec<_> = weights.into_iter().collect();
        weights.sort_unstable();
        weights.dedup();
        weights
            .into_iter()
            .map(|weight| font_weight_option(weight, default))
            .collect()
    }

    pub fn weight_label(&self, family: &str, weight: u16) -> String {
        self.weight_options(family)
            .into_iter()
            .find(|option| option.value == weight)
            .map_or_else(|| font_weight_label(weight), |option| option.label)
    }

    pub fn resolve_ui_family(&self, requested: &str) -> String {
        self.resolve_family(
            requested,
            &["Microsoft YaHei UI", "Microsoft YaHei", "Segoe UI"],
            false,
        )
    }

    pub fn resolve_data_family(&self, requested: &str) -> String {
        self.resolve_family(requested, &["Cascadia Mono", "Consolas"], true)
    }

    pub fn apply(
        &self,
        context: &egui::Context,
        requested_ui: &str,
        requested_data: &str,
        requested_ui_weight: u16,
        requested_data_weight: u16,
        ui_size: f32,
    ) -> AppliedFonts {
        let ui_family = self.resolve_ui_family(requested_ui);
        let data_family = self.resolve_data_family(requested_data);
        let ui_weight = self.resolve_weight(&ui_family, requested_ui_weight);
        let data_weight = self.resolve_weight(&data_family, requested_data_weight);
        let mut definitions = FontDefinitions::default();
        let mut warnings = Vec::new();

        if let Some(font) = self.font_data(&ui_family, ui_weight) {
            definitions
                .font_data
                .insert(UI_FONT_KEY.into(), Arc::new(font));
            definitions
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, UI_FONT_KEY.into());
        } else {
            warnings.push(format!("无法载入界面字体 {ui_family}，已使用内置字体"));
        }

        if let Some(font) = self.font_data(&data_family, data_weight) {
            definitions
                .font_data
                .insert(DATA_FONT_KEY.into(), Arc::new(font));
            definitions.families.insert(
                data_font_family(),
                vec![DATA_FONT_KEY.into(), UI_FONT_KEY.into(), "Hack".into()],
            );
        } else {
            warnings.push(format!(
                "无法载入数据字体 {data_family}，已使用内置等宽字体"
            ));
            let fallback = definitions
                .families
                .get(&FontFamily::Monospace)
                .cloned()
                .unwrap_or_default();
            definitions.families.insert(data_font_family(), fallback);
        }

        context.set_fonts(definitions);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let mut style = (*context.style_of(theme)).clone();
            style.text_styles.insert(
                TextStyle::Body,
                FontId::new(ui_size, FontFamily::Proportional),
            );
            style.text_styles.insert(
                TextStyle::Button,
                FontId::new(ui_size, FontFamily::Proportional),
            );
            style.text_styles.insert(
                TextStyle::Small,
                FontId::new((ui_size - 2.0).max(10.0), FontFamily::Proportional),
            );
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            context.set_style_of(theme, style);
        }

        AppliedFonts {
            ui_family,
            data_family,
            ui_weight,
            data_weight,
            warnings,
        }
    }

    fn resolve_family(&self, requested: &str, fallbacks: &[&str], monospace: bool) -> String {
        let candidates = if monospace {
            &self.mono_families
        } else {
            &self.ui_families
        };
        if let Some(name) = find_case_insensitive(candidates, requested) {
            return name;
        }
        for fallback in fallbacks {
            if let Some(name) = find_case_insensitive(candidates, fallback) {
                return name;
            }
        }
        candidates.first().cloned().unwrap_or_default()
    }

    fn weight_profile(&self, family: &str) -> Option<WeightProfile> {
        if let Some(profile) = self.weight_profiles.borrow().get(family) {
            return profile.clone();
        }

        let profile = self.discover_weight_profile(family);
        self.weight_profiles
            .borrow_mut()
            .insert(family.to_owned(), profile.clone());
        profile
    }

    fn discover_weight_profile(&self, family: &str) -> Option<WeightProfile> {
        let faces = self.faces.get(family)?;
        for &face in faces {
            let Some(font) = self.load_font(face.id) else {
                continue;
            };
            let Some(axis) = font
                .variation_axes()
                .into_iter()
                .find(|axis| axis.tag.to_be_bytes() == *b"wght")
            else {
                continue;
            };
            if !axis.range.min.is_finite()
                || !axis.range.max.is_finite()
                || axis.range.min > axis.range.max
            {
                continue;
            }
            let default = normalize_weight(axis.default, axis.range.min, axis.range.max);
            return Some(WeightProfile::Variable {
                face,
                min: axis.range.min,
                max: axis.range.max,
                default,
            });
        }

        let default = faces
            .iter()
            .min_by_key(|face| face.weight.abs_diff(DEFAULT_FONT_WEIGHT))
            .map_or(DEFAULT_FONT_WEIGHT, |face| face.weight);
        Some(WeightProfile::Static {
            faces: faces.clone(),
            default,
        })
    }

    fn resolve_weight(&self, family: &str, requested: u16) -> u16 {
        match self.weight_profile(family) {
            Some(WeightProfile::Variable {
                min, max, default, ..
            }) => resolve_variable_weight(requested, min, max, default),
            Some(WeightProfile::Static { faces, default }) => {
                resolve_static_weight(requested, default, faces.iter().map(|face| face.weight))
            }
            None => DEFAULT_FONT_WEIGHT,
        }
    }

    fn font_data(&self, family: &str, weight: u16) -> Option<FontData> {
        match self.weight_profile(family)? {
            WeightProfile::Variable { face, .. } => {
                let mut font = self.load_font(face.id)?;
                font.tweak.coords = VariationCoords::new([(b"wght", f32::from(weight))]);
                Some(font)
            }
            WeightProfile::Static { faces, default } => {
                let face = faces
                    .iter()
                    .find(|face| face.weight == weight)
                    .or_else(|| faces.iter().find(|face| face.weight == default))?;
                self.load_font(face.id)
            }
        }
    }

    fn load_font(&self, id: ID) -> Option<FontData> {
        let (bytes, face_index) = self
            .database
            .with_face_data(id, |data, index| (data.to_vec(), index))?;
        let mut font = FontData::from_owned(bytes);
        font.index = face_index;
        Some(font)
    }
}

pub fn data_font_family() -> FontFamily {
    FontFamily::Name(DATA_FAMILY_KEY.into())
}

pub fn font_weight_label(weight: u16) -> String {
    match weight {
        100 => "Thin".into(),
        200 => "Extra Light".into(),
        300 => "Light".into(),
        400 => "Regular".into(),
        500 => "Medium".into(),
        600 => "SemiBold".into(),
        700 => "Bold".into(),
        800 => "ExtraBold".into(),
        900 => "Black".into(),
        _ => weight.to_string(),
    }
}

fn font_weight_option(value: u16, default: u16) -> FontWeightOption {
    let label = if value == default && !STANDARD_FONT_WEIGHTS.contains(&value) {
        format!("Default ({value})")
    } else {
        font_weight_label(value)
    };
    FontWeightOption { value, label }
}

fn variable_weight_options(min: f32, max: f32, default: u16) -> Vec<u16> {
    let mut weights: Vec<_> = STANDARD_FONT_WEIGHTS
        .into_iter()
        .filter(|weight| min <= f32::from(*weight) && f32::from(*weight) <= max)
        .collect();
    if min <= f32::from(default) && f32::from(default) <= max {
        weights.push(default);
    }
    weights
}

fn normalize_weight(value: f32, min: f32, max: f32) -> u16 {
    value.clamp(min, max).round().clamp(1.0, 1000.0) as u16
}

fn resolve_variable_weight(requested: u16, min: f32, max: f32, default: u16) -> u16 {
    let requested_value = f32::from(requested);
    if min <= requested_value && requested_value <= max {
        requested
    } else {
        default
    }
}

fn resolve_static_weight(
    requested: u16,
    default: u16,
    available: impl IntoIterator<Item = u16>,
) -> u16 {
    available
        .into_iter()
        .find(|weight| *weight == requested)
        .unwrap_or(default)
}

fn find_case_insensitive(candidates: &[String], requested: &str) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(requested))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_options_include_named_weights_and_nonstandard_default() {
        assert_eq!(
            variable_weight_options(250.0, 750.0, 450),
            vec![300, 400, 500, 600, 700, 450]
        );
    }

    #[test]
    fn weight_labels_cover_common_css_weights() {
        assert_eq!(font_weight_label(400), "Regular");
        assert_eq!(font_weight_label(500), "Medium");
        assert_eq!(font_weight_label(700), "Bold");
        assert_eq!(font_weight_label(450), "450");
        assert_eq!(font_weight_option(330, 330).label, "Default (330)");
    }

    #[test]
    fn variable_default_is_clamped_to_supported_range() {
        assert_eq!(normalize_weight(50.0, 200.0, 800.0), 200);
        assert_eq!(normalize_weight(950.0, 200.0, 800.0), 800);
    }

    #[test]
    fn unsupported_weights_fall_back_to_each_fonts_default() {
        assert_eq!(resolve_variable_weight(500, 200.0, 700.0, 330), 500);
        assert_eq!(resolve_variable_weight(900, 200.0, 700.0, 330), 330);
        assert_eq!(resolve_static_weight(700, 400, [400, 600, 700]), 700);
        assert_eq!(resolve_static_weight(500, 400, [400, 600, 700]), 400);
    }
}
