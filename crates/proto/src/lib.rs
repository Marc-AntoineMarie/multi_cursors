//! Vocabulaire commun entre machines : messages, boutons, géométrie annoncée.
//!
//! Choix de conception qui comptent pour la latence et la robustesse :
//!
//! * **Les positions sont absolues**, jamais des deltas. Une trame perdue ou arrivée en retard n'a
//!   donc aucun effet durable : le message suivant remet le curseur au bon endroit. C'est aussi ce
//!   qui permettra de passer en UDP plus tard sans rien changer à la logique.
//! * **Coordonnées locales à l'écran destinataire.** Un client ne connaît pas le plan global ; il
//!   reçoit des pixels directement utilisables pour son propre bureau.
//! * Cadrage longueur sur 4 octets + `postcard` : compact (un déplacement tient en une dizaine
//!   d'octets) et sans ambiguïté sur les limites de message.

use serde::{Deserialize, Serialize};

pub mod codec;

/// Version du protocole. Incrémentée à chaque changement incompatible de `Msg`.
pub const PROTOCOL_VERSION: u16 = 1;

/// Port d'écoute par défaut. Volontairement différent de 24800 (Synergy/Deskflow) pour pouvoir
/// faire tourner les deux en parallèle pendant les comparaisons.
pub const DEFAULT_PORT: u16 = 24810;

/// Taille maximale d'un message, garde-fou contre un cadrage corrompu.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    /// Bouton latéral « précédent ».
    Side,
    /// Bouton latéral « suivant ».
    Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
    Other,
}

impl Os {
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Other
        }
    }
}

/// Géométrie annoncée par une machine : la taille de **tout** son bureau.
///
/// Une machine à plusieurs moniteurs annonce le rectangle englobant, car c'est sur cette zone que
/// le compositeur mappe un pointeur absolu. Le découpage par moniteur viendra avec l'injection
/// mappée sur une sortie précise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u16,
    /// Nom de la machine, utilisé pour la faire correspondre à un écran de la disposition.
    pub name: String,
    pub os: Os,
    pub geometry: Geometry,
    /// Secret partagé de la V1. Remplacé par TLS + appariement PIN à l'étape 6.
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    ProtocolMismatch { server: u16 },
    BadToken,
    UnknownMachine { name: String },
    AlreadyConnected { name: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ProtocolMismatch { server } => write!(
                f,
                "versions de protocole incompatibles : le serveur parle la version {server}, pas la nôtre ({PROTOCOL_VERSION})"
            ),
            Refusal::BadToken => write!(f, "jeton refusé : il doit être identique dans la configuration des deux machines"),
            Refusal::UnknownMachine { name } => write!(
                f,
                "la machine « {name} » n'apparaît dans aucun écran de la disposition du serveur"
            ),
            Refusal::AlreadyConnected { name } => {
                write!(f, "une autre connexion utilise déjà le nom « {name} »")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    /// Client → serveur, premier message de la connexion.
    Hello(Hello),
    /// Serveur → client, connexion acceptée. `screen` est l'identifiant attribué au client.
    Welcome { screen: u32, server_name: String },
    /// Serveur → client, connexion refusée (avec la raison, pour un message d'erreur utile).
    Refused(Refusal),

    /// Le curseur arrive sur cet écran, à cette position locale.
    Enter { x: i32, y: i32 },
    /// Le curseur quitte cet écran. Les boutons encore enfoncés sont relâchés par le client.
    Leave,
    /// Déplacement : position absolue en pixels, locale à l'écran du client.
    MoveTo { x: i32, y: i32 },
    Button { button: Button, pressed: bool },
    /// Molette en crans. `y > 0` défile vers le haut, `x > 0` vers la droite.
    Scroll { x: i32, y: i32 },

    /// Mesure d'aller-retour, affichée comme latence dans l'interface.
    Ping { nonce: u64 },
    Pong { nonce: u64 },

    /// Demande de prise du rôle serveur par une autre machine (étape 5).
    Claim { name: String },
    /// Fermeture propre : le pair relâche tout.
    Bye,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour_serialisation() {
        let msgs = vec![
            Msg::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                name: "portable".into(),
                os: Os::Linux,
                geometry: Geometry {
                    width: 1920,
                    height: 1080,
                },
                token: "secret".into(),
            }),
            Msg::Welcome {
                screen: 2,
                server_name: "portable".into(),
            },
            Msg::MoveTo { x: -3, y: 1079 },
            Msg::Button {
                button: Button::Middle,
                pressed: true,
            },
            Msg::Scroll { x: 0, y: -2 },
            Msg::Leave,
            Msg::Refused(Refusal::BadToken),
        ];
        for msg in msgs {
            let bytes = postcard::to_stdvec(&msg).unwrap();
            let back: Msg = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn un_deplacement_reste_minuscule() {
        // La taille du message conditionne la latence sur un lien chargé : on vérifie qu'un
        // déplacement tient dans une poignée d'octets.
        let bytes = postcard::to_stdvec(&Msg::MoveTo { x: 1280, y: 720 }).unwrap();
        assert!(bytes.len() <= 8, "déplacement de {} octets", bytes.len());
    }
}
