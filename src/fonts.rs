use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId, TextStyle};
use fontdb::{Database, ID, Style};

const UI_FONT_KEY: &str = "escom_ui_font";
const DATA_FONT_KEY: &str = "escom_data_font";
const DATA_FAMILY_KEY: &str = "escom_data_family";

pub struct FontCatalog {
    database: Database,
    faces: BTreeMap<String, ID>,
    ui_families: Vec<String>,
    mono_families: Vec<String>,
}

pub struct AppliedFonts {
    pub ui_family: String,
    pub data_family: String,
    pub warnings: Vec<String>,
}

impl FontCatalog {
    pub fn load() -> Self {
        let mut database = Database::new();
        database.load_system_fonts();

        let mut preferred_faces = BTreeMap::new();
        let mut fallback_faces = BTreeMap::new();
        let mut mono_names = BTreeSet::new();
        for face in database.faces() {
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            fallback_faces.entry(family.clone()).or_insert(face.id);
            if face.style == Style::Normal {
                preferred_faces.entry(family.clone()).or_insert(face.id);
            }
            if face.monospaced {
                mono_names.insert(family.clone());
            }
        }
        for (family, id) in fallback_faces {
            preferred_faces.entry(family).or_insert(id);
        }

        let ui_families = preferred_faces.keys().cloned().collect();
        let mono_families = mono_names
            .into_iter()
            .filter(|name| preferred_faces.contains_key(name))
            .collect();

        Self {
            database,
            faces: preferred_faces,
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
        ui_size: f32,
    ) -> AppliedFonts {
        let ui_family = self.resolve_ui_family(requested_ui);
        let data_family = self.resolve_data_family(requested_data);
        let mut definitions = FontDefinitions::default();
        let mut warnings = Vec::new();

        if let Some(font) = self.font_data(&ui_family) {
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

        if let Some(font) = self.font_data(&data_family) {
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

    fn font_data(&self, family: &str) -> Option<FontData> {
        let id = *self.faces.get(family)?;
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

fn find_case_insensitive(candidates: &[String], requested: &str) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(requested))
        .cloned()
}
