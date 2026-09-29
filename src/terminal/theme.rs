//! Terminal palette presets from the MIT-licensed iTerm2 Color Schemes collection.
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_through_settings_and_wire_protocol() {
        for &(theme, _, id) in TerminalTheme::PRESETS {
            assert_eq!(serde_json::to_value(theme).unwrap(), id);
            let wire = postcard::to_allocvec(&theme).unwrap();
            assert_eq!(postcard::from_bytes::<TerminalTheme>(&wire).unwrap(), theme);
            let palette = theme.palette();
            let custom = TerminalTheme::Custom(palette);
            assert_eq!(
                serde_json::from_value::<TerminalTheme>(serde_json::to_value(custom).unwrap())
                    .unwrap(),
                custom
            );
            assert_eq!(
                postcard::from_bytes::<TerminalTheme>(&postcard::to_allocvec(&custom).unwrap())
                    .unwrap(),
                custom
            );
        }
    }

    #[test]
    fn custom_colors_reject_invalid_hex_and_palette_lengths() {
        for invalid in ["red", "#fff", "#1234567", "#zz0000", "#é1234"] {
            assert!(serde_json::from_value::<ThemeColor>(serde_json::json!(invalid)).is_err());
        }
        let mut palette = serde_json::to_value(TerminalTheme::Dark.palette()).unwrap();
        palette["ansi"].as_array_mut().unwrap().pop();
        assert!(serde_json::from_value::<TerminalPalette>(palette).is_err());
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalTheme {
    Dark,
    Light,
    SolarizedDark,
    Dracula,
    CatppuccinMocha,
    CatppuccinMacchiato,
    CatppuccinFrappe,
    CatppuccinLatte,
    Nord,
    GruvboxDark,
    GruvboxLight,
    TokyoNight,
    TokyoNightStorm,
    TokyoNightDay,
    RosePine,
    RosePineMoon,
    RosePineDawn,
    KanagawaWave,
    KanagawaLotus,
    OneDark,
    GithubDark,
    GithubLight,
    EverforestDark,
    EverforestLight,
    SolarizedLight,
    Custom(TerminalPalette),
    /// Resolved by the UI before crossing the terminal-host boundary.
    #[default]
    Auto,
}

/// Unknown keys are ignored rather than refused: a newer build may add a
/// color, and settings keep what this build does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPalette {
    pub foreground: ThemeColor,
    pub background: ThemeColor,
    pub cursor: ThemeColor,
    pub ansi: [ThemeColor; 16],
    #[serde(default)]
    pub light: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColor(pub(crate) u32);
impl ThemeColor {
    pub fn rgb(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }
}
impl Serialize for ThemeColor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("#{:06x}", self.0))
    }
}
impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let hex = value
            .strip_prefix('#')
            .filter(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| serde::de::Error::custom("expected #RRGGBB color"))?;
        Ok(Self(
            u32::from_str_radix(hex, 16).map_err(serde::de::Error::custom)?,
        ))
    }
}

impl TerminalTheme {
    pub const PRESETS: &'static [(Self, &'static str, &'static str)] = &[
        (Self::Auto, "Follow app theme", "auto"),
        (Self::Dark, "Dark", "dark"),
        (Self::Light, "Light", "light"),
        (Self::SolarizedDark, "Solarized dark", "solarized_dark"),
        (Self::Dracula, "Dracula", "dracula"),
        (
            Self::CatppuccinMocha,
            "Catppuccin Mocha",
            "catppuccin_mocha",
        ),
        (
            Self::CatppuccinMacchiato,
            "Catppuccin Macchiato",
            "catppuccin_macchiato",
        ),
        (
            Self::CatppuccinFrappe,
            "Catppuccin Frappe",
            "catppuccin_frappe",
        ),
        (
            Self::CatppuccinLatte,
            "Catppuccin Latte",
            "catppuccin_latte",
        ),
        (Self::Nord, "Nord", "nord"),
        (Self::GruvboxDark, "Gruvbox Dark", "gruvbox_dark"),
        (Self::GruvboxLight, "Gruvbox Light", "gruvbox_light"),
        (Self::TokyoNight, "TokyoNight", "tokyo_night"),
        (
            Self::TokyoNightStorm,
            "TokyoNight Storm",
            "tokyo_night_storm",
        ),
        (Self::TokyoNightDay, "TokyoNight Day", "tokyo_night_day"),
        (Self::RosePine, "Rose Pine", "rose_pine"),
        (Self::RosePineMoon, "Rose Pine Moon", "rose_pine_moon"),
        (Self::RosePineDawn, "Rose Pine Dawn", "rose_pine_dawn"),
        (Self::KanagawaWave, "Kanagawa Wave", "kanagawa_wave"),
        (Self::KanagawaLotus, "Kanagawa Lotus", "kanagawa_lotus"),
        (Self::OneDark, "Atom One Dark", "one_dark"),
        (Self::GithubDark, "GitHub Dark Default", "github_dark"),
        (Self::GithubLight, "GitHub Light Default", "github_light"),
        (
            Self::EverforestDark,
            "Everforest Dark Hard",
            "everforest_dark",
        ),
        (
            Self::EverforestLight,
            "Everforest Light Med",
            "everforest_light",
        ),
        (
            Self::SolarizedLight,
            "iTerm2 Solarized Light",
            "solarized_light",
        ),
    ];
    pub fn id(self) -> &'static str {
        Self::PRESETS
            .iter()
            .find(|(theme, _, _)| *theme == self)
            .map_or("custom", |(_, _, id)| *id)
    }
    pub fn palette(self) -> TerminalPalette {
        let (foreground, background, ansi, light) = match self {
            Self::Auto => return Self::Dark.palette(),
            Self::Dark => (
                0xd8d8d8,
                0x101010,
                [
                    0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                    0x4c566a, 0xd08770, 0xb1d196, 0xf0d399, 0x8fbcdb, 0xc5a0c0, 0x93ccdc, 0xeceff4,
                ],
                false,
            ),
            Self::Light => (
                0x24292f,
                0xffffff,
                [
                    0x24292f, 0xcf222e, 0x116329, 0x7d4e00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                    0x57606a, 0xa40e26, 0x1a7f37, 0x9a6700, 0x0550ae, 0x6639ba, 0x0a6269, 0x8c959f,
                ],
                true,
            ),
            Self::SolarizedDark => (
                0x839496,
                0x002b36,
                [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
                false,
            ),
            Self::Dracula => (
                0xf8f8f2,
                0x282a36,
                [
                    0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                    0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
                ],
                false,
            ),
            Self::CatppuccinMocha => (
                0xcdd6f4,
                0x1e1e2e,
                [
                    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
                    0x585b70, 0xf37799, 0x89d88b, 0xebd391, 0x74a8fc, 0xf2aede, 0x6bd7ca, 0xbac2de,
                ],
                false,
            ),
            Self::CatppuccinMacchiato => (
                0xcad3f5,
                0x24273a,
                [
                    0x494d64, 0xed8796, 0xa6da95, 0xeed49f, 0x8aadf4, 0xf5bde6, 0x8bd5ca, 0xa5adcb,
                    0x5b6078, 0xec7486, 0x8ccf7f, 0xe1c682, 0x78a1f6, 0xf2a9dd, 0x63cbc0, 0xb8c0e0,
                ],
                false,
            ),
            Self::CatppuccinFrappe => (
                0xc6d0f5,
                0x303446,
                [
                    0x51576d, 0xe78284, 0xa6d189, 0xe5c890, 0x8caaee, 0xf4b8e4, 0x81c8be, 0xa5adce,
                    0x626880, 0xe67172, 0x8ec772, 0xd9ba73, 0x7b9ef0, 0xf2a4db, 0x5abfb5, 0xb5bfe2,
                ],
                false,
            ),
            Self::CatppuccinLatte => (
                0x4c4f69,
                0xeff1f5,
                [
                    0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
                    0x6c6f85, 0xde293e, 0x49af3d, 0xeea02d, 0x456eff, 0xfe85d8, 0x2d9fa8, 0xbcc0cc,
                ],
                true,
            ),
            Self::Nord => (
                0xd8dee9,
                0x2e3440,
                [
                    0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                    0x596377, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
                ],
                false,
            ),
            Self::GruvboxDark => (
                0xebdbb2,
                0x282828,
                [
                    0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                    0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
                ],
                false,
            ),
            Self::GruvboxLight => (
                0x3c3836,
                0xfbf1c7,
                [
                    0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
                    0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
                ],
                true,
            ),
            Self::TokyoNight => (
                0xc0caf5,
                0x1a1b26,
                [
                    0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                    0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
                ],
                false,
            ),
            Self::TokyoNightStorm => (
                0xc0caf5,
                0x24283b,
                [
                    0x1d202f, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                    0x4e5575, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
                ],
                false,
            ),
            Self::TokyoNightDay => (
                0x3760bf,
                0xe1e2e7,
                [
                    0xe9e9ed, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x6172b0,
                    0xa1a6c5, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x3760bf,
                ],
                true,
            ),
            Self::RosePine => (
                0xe0def4,
                0x191724,
                [
                    0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
                    0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
                ],
                false,
            ),
            Self::RosePineMoon => (
                0xe0def4,
                0x232136,
                [
                    0x393552, 0xeb6f92, 0x3e8fb0, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xea9a97, 0xe0def4,
                    0x6e6a86, 0xeb6f92, 0x3e8fb0, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xea9a97, 0xe0def4,
                ],
                false,
            ),
            Self::RosePineDawn => (
                0x575279,
                0xfaf4ed,
                [
                    0xf2e9e1, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
                    0x9893a5, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
                ],
                true,
            ),
            Self::KanagawaWave => (
                0xdcd7ba,
                0x1f1f28,
                [
                    0x090618, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
                    0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
                ],
                false,
            ),
            Self::KanagawaLotus => (
                0x545464,
                0xf2ecbc,
                [
                    0x1f1f28, 0xc84053, 0x6f894e, 0x77713f, 0x4d699b, 0xb35b79, 0x597b75, 0x545464,
                    0x8a8980, 0xd7474b, 0x6e915f, 0x836f4a, 0x6693bf, 0x624c83, 0x5e857a, 0x43436c,
                ],
                true,
            ),
            Self::OneDark => (
                0xabb2bf,
                0x21252b,
                [
                    0x21252b, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                    0x767676, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                ],
                false,
            ),
            Self::GithubDark => (
                0xe6edf3,
                0x0d1117,
                [
                    0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                    0x6e7681, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
                ],
                false,
            ),
            Self::GithubLight => (
                0x1f2328,
                0xffffff,
                [
                    0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                    0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
                ],
                true,
            ),
            Self::EverforestDark => (
                0xd3c6aa,
                0x1e2326,
                [
                    0x7a8478, 0xe67e80, 0xa7c080, 0xdbbc7f, 0x7fbbb3, 0xd699b6, 0x83c092, 0xf2efdf,
                    0xa6b0a0, 0xf85552, 0x8da101, 0xdfa000, 0x3a94c5, 0xdf69ba, 0x35a77c, 0xfffbef,
                ],
                false,
            ),
            Self::EverforestLight => (
                0x5c6a72,
                0xefebd4,
                [
                    0x7a8478, 0xe67e80, 0x9ab373, 0xc1a266, 0x7fbbb3, 0xd699b6, 0x83c092, 0xb2af9f,
                    0xa6b0a0, 0xf85552, 0x8da101, 0xdfa000, 0x3a94c5, 0xdf69ba, 0x35a77c, 0xfffbef,
                ],
                true,
            ),
            Self::SolarizedLight => (
                0x657b83,
                0xfdf6e3,
                [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xbbb5a2,
                    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
                true,
            ),
            Self::Custom(palette) => return palette,
        };
        TerminalPalette {
            foreground: ThemeColor(foreground),
            background: ThemeColor(background),
            cursor: ThemeColor(foreground),
            ansi: ansi.map(ThemeColor),
            light,
        }
    }
}
