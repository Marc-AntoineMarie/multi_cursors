//! Capture et injection d'événements de pointage, derrière une abstraction unique.
//!
//! Les traits de ce module sont la frontière qui rend l'application portable : le moteur
//! (`mc-node`) ne parle jamais à evdev, uinput, `SendInput` ni `CGEventPost` directement. Ajouter
//! une plateforme consiste à fournir une implémentation de `PointerSink` (recevoir le curseur)
//! puis, plus tard, de la capture (le donner), sans toucher à la logique de bascule.

use anyhow::Result;
use mc_proto::Button;

#[cfg(target_os = "linux")]
pub mod sys_linux;

#[cfg(target_os = "linux")]
mod capture_linux;
#[cfg(target_os = "linux")]
mod sink_linux;
#[cfg(target_os = "windows")]
mod sink_windows;

#[cfg(target_os = "linux")]
pub use capture_linux::{list_capturable, Capture, CaptureControl, VIRTUAL_PREFIX};
#[cfg(target_os = "linux")]
pub use sink_linux::UinputPointer;
#[cfg(target_os = "windows")]
pub use sink_windows::WindowsPointer;

/// Événement brut tel que produit par un périphérique capturé, avant toute décision d'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEvent {
    /// Déplacement relatif en pixels du bureau virtuel.
    Motion { dx: i32, dy: i32 },
    Button { button: Button, pressed: bool },
    /// Molette en crans.
    Scroll { x: i32, y: i32 },
}

/// Réglages du comportement de la capture.
///
/// Indépendant de la plateforme : les notions de sensibilité et de pavé tactile existent partout,
/// et la configuration doit pouvoir les exprimer même sur une machine qui ne sait pas encore
/// capturer (un client Windows, par exemple).
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// Multiplicateur global appliqué à tous les déplacements.
    pub sensitivity: f64,
    /// Pixels parcourus par millimètre de doigt sur le pavé tactile.
    pub touchpad_pixels_per_mm: f64,
    /// Millimètres de doigt par cran de molette, pour le défilement à deux doigts.
    pub touchpad_mm_per_scroll: f64,
    pub tap_to_click: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            touchpad_pixels_per_mm: 12.0,
            touchpad_mm_per_scroll: 4.0,
            tap_to_click: true,
        }
    }
}

/// Destination d'événements de pointage : la machine qui possède le curseur à cet instant.
pub trait PointerSink: Send {
    /// Position absolue en pixels, locale au bureau de cette machine.
    fn move_to(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, button: Button, pressed: bool) -> Result<()>;
    fn scroll(&mut self, x: i32, y: i32) -> Result<()>;
    /// Relâche tout bouton encore enfoncé. Appelé quand le curseur quitte cette machine.
    fn release_all(&mut self) -> Result<()>;
}

/// Crée la sortie pointeur de la plateforme courante.
///
/// C'est le seul endroit où le choix de plateforme est fait pour l'injection ; tout le reste du
/// code manipule un `PointerSink`.
pub fn create_pointer_sink(name: &str, width: i32, height: i32) -> Result<Box<dyn PointerSink>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(UinputPointer::new(name, width, height)?))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = name;
        Ok(Box::new(WindowsPointer::new(width, height)?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (name, width, height);
        anyhow::bail!(
            "injection du curseur non implémentée sur cette plateforme (macOS prévu à l'étape 3)"
        )
    }
}

/// Taille du bureau telle que la plateforme la rapporte, si elle sait la donner.
pub fn desktop_geometry() -> Option<(i32, i32)> {
    #[cfg(target_os = "linux")]
    {
        sys_linux::drm_geometry()
    }
    #[cfg(target_os = "windows")]
    {
        sink_windows::virtual_desktop_size().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Une vérification élémentaire affichée par `mcd check`.
#[derive(Debug, Clone)]
pub struct CheckItem {
    pub label: String,
    pub ok: bool,
    /// Précision facultative : liste de périphériques, message système…
    pub detail: Option<String>,
}

/// Diagnostic des droits, volontairement sans effet de bord.
///
/// C'est la première commande à lancer sur une machine neuve, et elle doit rester sûre même si un
/// autre MultiCursos tourne déjà. Le contenu est propre à chaque plateforme, la présentation non :
/// `mcd` se contente d'afficher les éléments et les étapes de correction.
#[derive(Debug, Clone, Default)]
pub struct Permissions {
    pub items: Vec<CheckItem>,
    /// Étapes à suivre, déjà rédigées pour la plateforme courante.
    pub remediation: Vec<String>,
}

impl Permissions {
    pub fn ok(&self) -> bool {
        self.items.iter().all(|item| item.ok)
    }

    fn push(&mut self, label: &str, ok: bool, detail: Option<String>) {
        self.items.push(CheckItem {
            label: label.to_string(),
            ok,
            detail,
        });
    }
}

#[cfg(target_os = "linux")]
pub fn check_permissions() -> Permissions {
    use std::fs::OpenOptions;

    let mut perms = Permissions::default();

    let present = std::path::Path::new("/dev/uinput").exists();
    let writable = OpenOptions::new().write(true).open("/dev/uinput").is_ok();
    perms.push("module uinput chargé (/dev/uinput)", present, None);
    perms.push("/dev/uinput accessible en écriture", writable, None);

    let mut readable = Vec::new();
    let mut unreadable = Vec::new();
    if let Ok(devices) = list_capturable() {
        for d in devices {
            let label = format!("{} ({})", d.name, d.path.display());
            if std::fs::File::open(&d.path).is_ok() {
                readable.push(label);
            } else {
                unreadable.push(label);
            }
        }
    }
    perms.push(
        "périphériques de pointage lisibles",
        !readable.is_empty(),
        Some(if readable.is_empty() {
            "aucun".to_string()
        } else {
            readable.join(", ")
        }),
    );
    if !unreadable.is_empty() {
        perms.push(
            "périphériques inaccessibles",
            false,
            Some(unreadable.join(", ")),
        );
    }

    if !present {
        perms.remediation.push(
            "charger le module uinput :\n    echo uinput | sudo tee /etc/modules-load.d/uinput.conf\n    sudo modprobe uinput".into(),
        );
    } else if !writable {
        perms.remediation.push(
            "ouvrir /dev/uinput au groupe input :\n    sudo install -m 644 packaging/60-multicursos.rules /etc/udev/rules.d/\n    sudo udevadm control --reload-rules && sudo udevadm trigger".into(),
        );
    }
    if !unreadable.is_empty() || readable.is_empty() {
        perms.remediation.push(
            "autoriser la lecture des périphériques d'entrée :\n    sudo usermod -aG input $USER\n  puis fermer la session et la réouvrir (l'appartenance à un groupe n'est relue qu'à la connexion ; `id` doit afficher « input »)".into(),
        );
    }

    perms
}

#[cfg(target_os = "windows")]
pub fn check_permissions() -> Permissions {
    let mut perms = Permissions::default();
    match sink_windows::virtual_desktop_size() {
        Ok((w, h)) => perms.push(
            "bureau virtuel détecté",
            true,
            Some(format!("{w}x{h} pixels")),
        ),
        Err(err) => perms.push("bureau virtuel détecté", false, Some(err.to_string())),
    }
    // `SendInput` est toujours disponible, mais il est filtré vers les fenêtres d'un niveau de
    // privilège supérieur : rien ne se passera au-dessus d'une invite UAC si le client n'est pas
    // lui-même élevé. Ce n'est pas un défaut de configuration, juste une limite à connaître.
    perms.push("injection par SendInput disponible", true, None);
    perms.remediation.push(
        "pour piloter une fenêtre lancée en administrateur, démarrer le client en administrateur lui aussi (restriction UIPI de Windows)".into(),
    );
    perms
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn check_permissions() -> Permissions {
    let mut perms = Permissions::default();
    perms.push("plateforme prise en charge", false, None);
    perms
        .remediation
        .push("seuls Linux et Windows sont implémentés à ce stade".into());
    perms
}
