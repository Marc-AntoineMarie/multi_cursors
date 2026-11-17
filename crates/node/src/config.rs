//! Configuration sur disque : qui je suis, qui sont les autres, où sont les écrans.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mc_layout::{Point, Screen, VirtualDesktop};
use mc_proto::{Geometry, DEFAULT_PORT};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    /// Nom de la machine. Doit correspondre au `name` de sa propre configuration.
    pub name: String,
    /// Coin supérieur gauche dans le plan global, en pixels.
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuningConfig {
    pub sensitivity: f64,
    pub touchpad_pixels_per_mm: f64,
    pub touchpad_mm_per_scroll: f64,
    pub tap_to_click: bool,
}

impl Default for TuningConfig {
    fn default() -> Self {
        let d = mc_input::Tuning::default();
        Self {
            sensitivity: d.sensitivity,
            touchpad_pixels_per_mm: d.touchpad_pixels_per_mm,
            touchpad_mm_per_scroll: d.touchpad_mm_per_scroll,
            tap_to_click: d.tap_to_click,
        }
    }
}

impl TuningConfig {
    pub fn to_tuning(&self) -> mc_input::Tuning {
        mc_input::Tuning {
            sensitivity: self.sensitivity,
            touchpad_pixels_per_mm: self.touchpad_pixels_per_mm,
            touchpad_mm_per_scroll: self.touchpad_mm_per_scroll,
            tap_to_click: self.tap_to_click,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Nom de **cette** machine. C'est la clé qui la relie à un écran de la disposition.
    pub name: String,
    /// Secret partagé, identique sur toutes les machines. Remplacé par TLS + PIN à l'étape 6.
    pub token: String,
    /// Adresse d'écoute quand cette machine tient le rôle serveur.
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub tuning: TuningConfig,
    /// Disposition des écrans. L'ordre fixe les identifiants (index dans la liste).
    #[serde(default, rename = "screen")]
    pub screens: Vec<ScreenConfig>,
}

fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("analyse de {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty(), "le champ `name` est vide");
        anyhow::ensure!(
            !self.token.trim().is_empty(),
            "le champ `token` est vide : il doit être identique sur toutes les machines"
        );
        for (i, s) in self.screens.iter().enumerate() {
            anyhow::ensure!(
                s.width > 1 && s.height > 1,
                "écran « {} » : dimensions invalides ({}x{})",
                s.name,
                s.width,
                s.height
            );
            if let Some(dup) = self.screens[..i].iter().find(|o| o.name == s.name) {
                anyhow::bail!("deux écrans portent le nom « {} »", dup.name);
            }
        }
        Ok(())
    }

    /// Construit la disposition : l'identifiant d'un écran est son index dans le fichier.
    pub fn virtual_desktop(&self) -> VirtualDesktop {
        VirtualDesktop::new(
            self.screens
                .iter()
                .enumerate()
                .map(|(i, s)| Screen {
                    id: i as u32,
                    name: s.name.clone(),
                    origin: Point::new(s.x, s.y),
                    width: s.width,
                    height: s.height,
                })
                .collect(),
        )
    }

    /// Géométrie de cette machine, telle qu'annoncée dans `Hello`.
    pub fn own_geometry(&self) -> Geometry {
        self.screens
            .iter()
            .find(|s| s.name == self.name)
            .map(|s| Geometry {
                width: s.width,
                height: s.height,
            })
            .unwrap_or_else(|| {
                let (width, height) = detect_geometry();
                Geometry { width, height }
            })
    }

    /// Configuration par défaut, avec la géométrie détectée et un jeton aléatoire.
    pub fn generate_default() -> Self {
        let name = hostname();
        let (width, height) = detect_geometry();
        Self {
            screens: vec![ScreenConfig {
                name: name.clone(),
                x: 0,
                y: 0,
                width,
                height,
            }],
            name,
            token: random_token(),
            listen: default_listen(),
            tuning: TuningConfig::default(),
        }
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!(
            "# Configuration MultiCursos.\n\
             #\n\
             # `name`  : nom de cette machine ; il doit apparaître dans un bloc [[screen]].\n\
             # `token` : identique sur toutes les machines, sinon la connexion est refusée.\n\
             #\n\
             # Chaque [[screen]] est une machine placée dans un plan global en pixels. Pour deux\n\
             # écrans côte à côte, le second commence en x = largeur du premier. Les bords qui se\n\
             # touchent deviennent les points de bascule du curseur.\n\
             #\n\
             # Une machine à plusieurs moniteurs déclare le rectangle qui les englobe : c'est la\n\
             # zone sur laquelle le compositeur projette un pointeur absolu.\n\
             \n{body}"
        )
    }
}

/// Emplacement du fichier de configuration, selon les conventions de la plateforme.
pub fn default_config_path() -> PathBuf {
    let base = if cfg!(windows) {
        // %APPDATA% existe sur toute session Windows ; HOME n'y est généralement pas défini.
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("multicursos")
        .join("config.toml")
}

/// Nom de la machine, utilisé comme valeur par défaut de `name`.
pub fn hostname() -> String {
    #[cfg(windows)]
    let from_system = std::env::var("COMPUTERNAME").ok();

    #[cfg(not(windows))]
    let from_system = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok();

    from_system
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "machine".to_string())
}

fn random_token() -> String {
    // 16 octets d'entropie du noyau suffisent pour un secret partagé de réseau local.
    // `read_exact` et pas `fs::read` : /dev/urandom est un flux sans fin.
    use std::io::Read;
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => "changez-moi".to_string(),
    }
}

/// Taille du bureau pour une configuration neuve.
///
/// Déléguée à la couche plateforme (`mc-input`), qui sait interroger sysfs DRM sous Linux et
/// `GetSystemMetrics` sous Windows. La valeur de repli n'est là que pour qu'une machine sans
/// écran — un NAS, par exemple — produise quand même un fichier valide.
pub fn detect_geometry() -> (i32, i32) {
    mc_input::desktop_geometry().unwrap_or((1920, 1080))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEMPLE: &str = r#"
name = "portable"
token = "abc"
listen = "0.0.0.0:24810"

[[screen]]
name = "portable"
width = 1920
height = 1080

[[screen]]
name = "bureau"
x = 1920
y = 0
width = 2560
height = 1440
"#;

    #[test]
    fn lecture_et_disposition() {
        let config: Config = toml::from_str(EXEMPLE).unwrap();
        config.validate().unwrap();
        assert_eq!(config.name, "portable");
        // Les valeurs de réglage absentes prennent leur défaut.
        assert_eq!(config.tuning.sensitivity, 1.0);

        let vd = config.virtual_desktop();
        assert_eq!(vd.screens().len(), 2);
        assert_eq!(vd.by_name("portable").unwrap().id, 0);
        assert_eq!(vd.by_name("bureau").unwrap().origin, Point::new(1920, 0));

        // La géométrie annoncée est bien celle de l'écran portant notre nom.
        assert_eq!(config.own_geometry().width, 1920);
    }

    #[test]
    fn noms_dupliques_refuses() {
        let config: Config = toml::from_str(
            r#"
name = "a"
token = "t"
[[screen]]
name = "a"
width = 800
height = 600
[[screen]]
name = "a"
width = 800
height = 600
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("deux écrans"), "message inattendu : {err}");
    }

    #[test]
    fn jeton_vide_refuse() {
        let config: Config = toml::from_str("name = \"a\"\ntoken = \"  \"\n").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn chemin_de_configuration_plausible() {
        let path = default_config_path();
        assert!(path.ends_with("multicursos/config.toml") || path.ends_with("multicursos\\config.toml"));
        // Jamais un chemin relatif nu quand l'environnement est normal.
        if std::env::var_os("HOME").is_some() || std::env::var_os("APPDATA").is_some() {
            assert!(path.is_absolute(), "chemin non absolu : {}", path.display());
        }
    }

    #[test]
    fn le_defaut_est_relisible() {
        let generated = Config::generate_default();
        generated.validate().unwrap();
        let text = generated.to_toml();
        let reread: Config = toml::from_str(&text).expect("le TOML produit doit être relisible");
        assert_eq!(reread.name, generated.name);
        assert_eq!(reread.screens.len(), 1);
    }
}
