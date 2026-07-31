use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use eframe::egui::Color32;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::settings;

pub const HIGHLIGHT_CONFIG_VERSION: u32 = 1;
pub const HIGHLIGHT_FILE_NAME: &str = "highlight.toml";

const DEFAULT_HIGHLIGHT_CONFIG: &str = r##"# ESCOM 高亮规则
#
# 修改后请在接收区点击“高亮”菜单中的“重新加载”。
# 规则按书写顺序匹配，第一条命中的规则负责整行样式。
# mode 可选 "contains"（包含文本）或 "regex"（正则表达式）。
# 颜色格式为 "#RRGGBB" 或带透明度的 "#RRGGBBAA"。

version = 1

[[rules]]
name = "错误"
enabled = true
mode = "regex"
pattern = '\b(ERROR|FATAL|PANIC|FAILED|FAILURE)\b|错误|失败'
case_sensitive = false
background = "#E5484D40"
underline = false

[[rules]]
name = "警告"
enabled = true
mode = "regex"
pattern = '\b(WARN|WARNING)\b|警告'
case_sensitive = false
background = "#E5A50A38"
underline = false

# 复制下面的规则即可添加自己的高亮条件。
[[rules]]
name = "自定义示例"
enabled = false
mode = "contains"
pattern = "READY"
case_sensitive = false
foreground = "#4CC9F0"
background = "#4CC9F026"
underline = true
"##;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum MatchMode {
    #[default]
    Contains,
    Regex,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HighlightConfig {
    version: u32,
    rules: Vec<HighlightRuleConfig>,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            version: HIGHLIGHT_CONFIG_VERSION,
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HighlightRuleConfig {
    name: String,
    enabled: bool,
    mode: MatchMode,
    pattern: String,
    case_sensitive: bool,
    foreground: Option<String>,
    background: Option<String>,
    underline: bool,
}

impl Default for HighlightRuleConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            mode: MatchMode::Contains,
            pattern: String::new(),
            case_sensitive: false,
            foreground: None,
            background: None,
            underline: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightStyle {
    pub foreground: Option<Color32>,
    pub background: Option<Color32>,
    pub underline: bool,
}

#[derive(Debug)]
struct CompiledRule {
    matcher: Regex,
    style: HighlightStyle,
}

#[derive(Debug)]
pub struct HighlightRules {
    path: PathBuf,
    rules: Vec<CompiledRule>,
}

impl HighlightRules {
    pub fn load_or_create() -> Result<Self, String> {
        load_or_create_at(&config_path())
    }

    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            rules: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn style_for(&self, text: &str) -> Option<HighlightStyle> {
        self.rules
            .iter()
            .find(|rule| rule.matcher.is_match(text))
            .map(|rule| rule.style)
    }
}

pub fn config_path() -> PathBuf {
    settings::settings_dir().join(HIGHLIGHT_FILE_NAME)
}

fn load_or_create_at(path: &Path) -> Result<HighlightRules, String> {
    create_default_if_missing(path)
        .map_err(|error| format!("无法创建高亮配置 {}：{error}", path.display()))?;
    let source = fs::read_to_string(path)
        .map_err(|error| format!("无法读取高亮配置 {}：{error}", path.display()))?;
    compile_config(
        path.to_path_buf(),
        source.strip_prefix('\u{feff}').unwrap_or(&source),
    )
}

fn create_default_if_missing(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(DEFAULT_HIGHLIGHT_CONFIG.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn compile_config(path: PathBuf, source: &str) -> Result<HighlightRules, String> {
    let config: HighlightConfig =
        toml::from_str(source).map_err(|error| format!("高亮配置 TOML 格式无效：{error}"))?;
    if config.version != HIGHLIGHT_CONFIG_VERSION {
        return Err(format!(
            "不支持的高亮配置版本 {}，当前支持版本 {}",
            config.version, HIGHLIGHT_CONFIG_VERSION
        ));
    }

    let mut rules = Vec::new();
    for (index, rule) in config.rules.into_iter().enumerate() {
        if !rule.enabled {
            continue;
        }
        let number = index + 1;
        let name = rule.name.trim();
        if name.is_empty() {
            return Err(format!("第 {number} 条高亮规则缺少 name"));
        }
        if rule.pattern.is_empty() {
            return Err(format!("高亮规则“{name}”的 pattern 不能为空"));
        }

        let expression = match rule.mode {
            MatchMode::Contains => regex::escape(&rule.pattern),
            MatchMode::Regex => rule.pattern,
        };
        let matcher = RegexBuilder::new(&expression)
            .case_insensitive(!rule.case_sensitive)
            .build()
            .map_err(|error| format!("高亮规则“{name}”的表达式无效：{error}"))?;
        let foreground = parse_optional_color(rule.foreground.as_deref(), name, "foreground")?;
        let background = parse_optional_color(rule.background.as_deref(), name, "background")?;
        if foreground.is_none() && background.is_none() && !rule.underline {
            return Err(format!(
                "高亮规则“{name}”至少需要 foreground、background 或 underline"
            ));
        }

        rules.push(CompiledRule {
            matcher,
            style: HighlightStyle {
                foreground,
                background,
                underline: rule.underline,
            },
        });
    }

    Ok(HighlightRules { path, rules })
}

fn parse_optional_color(
    value: Option<&str>,
    rule_name: &str,
    field_name: &str,
) -> Result<Option<Color32>, String> {
    value
        .map(|value| {
            parse_color(value).ok_or_else(|| {
                format!("高亮规则“{rule_name}”的 {field_name} 必须是 #RRGGBB 或 #RRGGBBAA")
            })
        })
        .transpose()
}

fn parse_color(value: &str) -> Option<Color32> {
    let hex = value.trim().strip_prefix('#')?;
    let parse_pair = |start: usize| u8::from_str_radix(&hex[start..start + 2], 16).ok();
    match hex.len() {
        6 => Some(Color32::from_rgb(
            parse_pair(0)?,
            parse_pair(2)?,
            parse_pair(4)?,
        )),
        8 => Some(Color32::from_rgba_unmultiplied(
            parse_pair(0)?,
            parse_pair(2)?,
            parse_pair(4)?,
            parse_pair(6)?,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(label: &str) -> PathBuf {
        let unique = format!(
            "escom-highlight-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn missing_config_is_created_and_compiled() {
        let directory = unique_test_dir("create");
        let path = directory.join(HIGHLIGHT_FILE_NAME);

        let rules = load_or_create_at(&path).unwrap();

        assert!(path.is_file());
        assert_eq!(rules.len(), 2);
        assert!(rules.style_for("fatal: device failed").is_some());
        assert!(rules.style_for("everything is fine").is_none());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn contains_rules_honor_case_sensitivity_and_colors() {
        let source = r##"
            version = 1

            [[rules]]
            name = "ready"
            mode = "contains"
            pattern = "READY"
            case_sensitive = true
            foreground = "#112233"
            background = "#44556677"
        "##;
        let rules = compile_config(PathBuf::from("highlight.toml"), source).unwrap();

        assert!(rules.style_for("READY now").is_some());
        assert!(rules.style_for("ready now").is_none());
        let style = rules.style_for("READY").unwrap();
        assert_eq!(style.foreground, Some(Color32::from_rgb(0x11, 0x22, 0x33)));
        assert_eq!(
            style.background,
            Some(Color32::from_rgba_unmultiplied(0x44, 0x55, 0x66, 0x77))
        );
    }

    #[test]
    fn invalid_rules_report_actionable_errors() {
        let bad_regex = r##"
            version = 1
            [[rules]]
            name = "broken"
            mode = "regex"
            pattern = "("
            foreground = "#FFFFFF"
        "##;
        assert!(
            compile_config(PathBuf::new(), bad_regex)
                .unwrap_err()
                .contains("broken")
        );

        let bad_color = r##"
            version = 1
            [[rules]]
            name = "color"
            pattern = "x"
            foreground = "red"
        "##;
        assert!(
            compile_config(PathBuf::new(), bad_color)
                .unwrap_err()
                .contains("#RRGGBB")
        );
    }
}
