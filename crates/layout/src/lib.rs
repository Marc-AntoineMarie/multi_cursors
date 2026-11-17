//! Géométrie du bureau virtuel.
//!
//! Tout le module est **pur** : aucune E/S, aucun état système. C'est volontaire — la logique de
//! bascule entre écrans est l'endroit où se nichent les bugs subtils (curseur qui entre au mauvais
//! endroit, qui se bloque dans un coin, qui saute d'un écran à l'autre), donc elle doit être
//! entièrement couverte par des tests unitaires rapides.
//!
//! Modèle : chaque machine possède **un rectangle** dans un plan global exprimé en pixels. Le
//! curseur a une position globale `Point` dont le démon serveur est le seul maître (voir
//! `mc-input` : on grabbe les périphériques réels et on réinjecte via un pointeur absolu uinput).
//! Déterminer l'écran actif n'est donc que de l'arithmétique sur cette position.

use serde::{Deserialize, Serialize};

/// Identifiant stable d'un écran dans la disposition (son index dans la configuration).
pub type ScreenId = u32;

/// Position en pixels dans le plan global du bureau virtuel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Un écran : une machine = un rectangle dans le plan global.
///
/// En V1 une machine expose un seul rectangle couvrant tout son bureau. Le découpage en plusieurs
/// moniteurs par machine viendra plus tard (il demande de mapper le pointeur absolu sur une sortie
/// précise côté injection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub id: ScreenId,
    /// Nom de la machine, tel qu'annoncé dans le message `Hello`.
    pub name: String,
    /// Coin supérieur gauche dans le plan global.
    pub origin: Point,
    pub width: i32,
    pub height: i32,
}

impl Screen {
    pub fn left(&self) -> i32 {
        self.origin.x
    }
    pub fn top(&self) -> i32 {
        self.origin.y
    }
    /// Première colonne **hors** de l'écran.
    pub fn right(&self) -> i32 {
        self.origin.x + self.width
    }
    /// Première ligne **hors** de l'écran.
    pub fn bottom(&self) -> i32 {
        self.origin.y + self.height
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left() && p.x < self.right() && p.y >= self.top() && p.y < self.bottom()
    }

    /// Ramène un point dans les limites de cet écran.
    pub fn clamp(&self, p: Point) -> Point {
        Point {
            x: p.x.clamp(self.left(), self.right() - 1),
            y: p.y.clamp(self.top(), self.bottom() - 1),
        }
    }

    /// Convertit une position globale en coordonnées locales à l'écran (ce qu'on envoie à la
    /// machine concernée, qui ne connaît pas le plan global).
    pub fn to_local(&self, p: Point) -> Point {
        Point {
            x: p.x - self.origin.x,
            y: p.y - self.origin.y,
        }
    }
}

/// Résultat d'un déplacement du curseur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Le curseur reste sur le même écran.
    Stay { screen: ScreenId, pos: Point },
    /// Le curseur change d'écran. `pos` est déjà la position d'entrée sur le nouvel écran.
    Cross {
        from: ScreenId,
        to: ScreenId,
        pos: Point,
    },
}

impl Motion {
    pub fn screen(&self) -> ScreenId {
        match *self {
            Motion::Stay { screen, .. } => screen,
            Motion::Cross { to, .. } => to,
        }
    }
    pub fn pos(&self) -> Point {
        match *self {
            Motion::Stay { pos, .. } | Motion::Cross { pos, .. } => pos,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VirtualDesktop {
    screens: Vec<Screen>,
}

impl VirtualDesktop {
    pub fn new(screens: Vec<Screen>) -> Self {
        Self { screens }
    }

    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }

    pub fn get(&self, id: ScreenId) -> Option<&Screen> {
        self.screens.iter().find(|s| s.id == id)
    }

    pub fn by_name(&self, name: &str) -> Option<&Screen> {
        self.screens.iter().find(|s| s.name == name)
    }

    pub fn screen_at(&self, p: Point) -> Option<&Screen> {
        self.screens.iter().find(|s| s.contains(p))
    }

    /// Cœur de la bascule : applique un déplacement et dit où atterrit le curseur.
    ///
    /// Trois cas, dans cet ordre :
    ///
    /// 1. la position candidate est encore sur l'écran courant → on y reste ;
    /// 2. elle tombe directement sur un autre écran → bascule à cette position exacte (le
    ///    mouvement est conservé intégralement, comme sur un seul grand bureau) ;
    /// 3. elle ne tombe sur aucun écran : on cherche le premier écran **traversé par le segment**
    ///    de déplacement. C'est indispensable pour les mouvements rapides — à 1000 Hz un coup de
    ///    poignet produit un delta de plusieurs milliers de pixels, qui peut enjamber entièrement
    ///    l'écran voisin. Sans ce calcul, le curseur resterait collé au bord au lieu de basculer.
    ///
    /// Si aucun écran n'est traversé (bord extérieur du bureau, ou trou entre deux écrans mal
    /// alignés), la position est bornée à l'écran courant : le curseur ne disparaît jamais.
    pub fn apply(&self, current: ScreenId, from: Point, dx: i32, dy: i32) -> Motion {
        let candidate = Point::new(from.x + dx, from.y + dy);

        if let Some(screen) = self.get(current) {
            if screen.contains(candidate) {
                return Motion::Stay {
                    screen: current,
                    pos: candidate,
                };
            }
        }

        if let Some(target) = self.screen_at(candidate) {
            return if target.id == current {
                Motion::Stay {
                    screen: target.id,
                    pos: candidate,
                }
            } else {
                Motion::Cross {
                    from: current,
                    to: target.id,
                    pos: candidate,
                }
            };
        }

        // Premier écran traversé par le segment [from, candidate].
        let mut best: Option<(f64, &Screen)> = None;
        for screen in self.screens.iter().filter(|s| s.id != current) {
            if let Some(t) = segment_entry(screen, from, candidate) {
                if best.map_or(true, |(bt, _)| t < bt) {
                    best = Some((t, screen));
                }
            }
        }
        if let Some((_, target)) = best {
            // On borne la position candidate dans l'écran d'arrivée plutôt que de s'arrêter au
            // point d'entrée : un geste franc vers la droite doit finir au bord droit de l'écran
            // de droite, exactement comme sur un bureau unique.
            return Motion::Cross {
                from: current,
                to: target.id,
                pos: target.clamp(candidate),
            };
        }

        match self.get(current) {
            Some(screen) => Motion::Stay {
                screen: current,
                pos: screen.clamp(candidate),
            },
            // Écran courant inconnu (disposition rechargée sous nos pieds) : on se replie sur le
            // premier écran disponible plutôt que de perdre le curseur.
            None => match self.screens.first() {
                Some(first) => Motion::Stay {
                    screen: first.id,
                    pos: first.clamp(candidate),
                },
                None => Motion::Stay {
                    screen: current,
                    pos: from,
                },
            },
        }
    }
}

/// Paramètre `t` dans `[0, 1]` auquel le segment `p0 -> p1` entre dans l'écran, s'il l'atteint.
///
/// Découpage par tranches (algorithme de Liang-Barsky) : pour chaque axe on calcule l'intervalle de
/// `t` pendant lequel le segment est dans la bande de l'écran, et on intersecte les deux.
fn segment_entry(screen: &Screen, p0: Point, p1: Point) -> Option<f64> {
    let d = (f64::from(p1.x - p0.x), f64::from(p1.y - p0.y));
    let mut t_enter = 0.0f64;
    let mut t_exit = 1.0f64;

    for (origin, delta, min, max) in [
        (
            f64::from(p0.x),
            d.0,
            f64::from(screen.left()),
            f64::from(screen.right() - 1),
        ),
        (
            f64::from(p0.y),
            d.1,
            f64::from(screen.top()),
            f64::from(screen.bottom() - 1),
        ),
    ] {
        if delta == 0.0 {
            // Déplacement parallèle à cette bande : soit on est dedans pour toute la durée, soit
            // on ne l'atteindra jamais.
            if origin < min || origin > max {
                return None;
            }
            continue;
        }
        let (mut a, mut b) = ((min - origin) / delta, (max - origin) / delta);
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        t_enter = t_enter.max(a);
        t_exit = t_exit.min(b);
        if t_enter > t_exit {
            return None;
        }
    }

    Some(t_enter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: ScreenId, x: i32, y: i32, w: i32, h: i32) -> Screen {
        Screen {
            id,
            name: format!("s{id}"),
            origin: Point::new(x, y),
            width: w,
            height: h,
        }
    }

    /// Deux écrans côte à côte : 0 = 1920x1080 à gauche, 1 = 2560x1440 à droite.
    fn side_by_side() -> VirtualDesktop {
        VirtualDesktop::new(vec![
            screen(0, 0, 0, 1920, 1080),
            screen(1, 1920, 0, 2560, 1440),
        ])
    }

    #[test]
    fn reste_sur_ecran_courant() {
        let vd = side_by_side();
        let m = vd.apply(0, Point::new(100, 100), 10, -20);
        assert_eq!(
            m,
            Motion::Stay {
                screen: 0,
                pos: Point::new(110, 80)
            }
        );
    }

    #[test]
    fn traverse_le_bord_droit() {
        let vd = side_by_side();
        let m = vd.apply(0, Point::new(1919, 500), 5, 0);
        assert_eq!(
            m,
            Motion::Cross {
                from: 0,
                to: 1,
                pos: Point::new(1924, 500)
            }
        );
        // Et la position locale sur le nouvel écran repart bien de son bord gauche.
        let target = vd.get(1).unwrap();
        assert_eq!(target.to_local(m.pos()), Point::new(4, 500));
    }

    #[test]
    fn traverse_puis_revient() {
        let vd = side_by_side();
        let aller = vd.apply(0, Point::new(1919, 300), 1, 0);
        assert_eq!(aller.screen(), 1);
        let retour = vd.apply(1, aller.pos(), -5, 0);
        assert_eq!(
            retour,
            Motion::Cross {
                from: 1,
                to: 0,
                pos: Point::new(1915, 300)
            }
        );
    }

    #[test]
    fn borne_au_bord_exterieur() {
        let vd = side_by_side();
        // Bord gauche du bureau : rien au-delà, le curseur doit coller au bord.
        let m = vd.apply(0, Point::new(3, 500), -50, 0);
        assert_eq!(
            m,
            Motion::Stay {
                screen: 0,
                pos: Point::new(0, 500)
            }
        );
        // Bord droit du bureau, sur l'écran de droite.
        let m = vd.apply(1, Point::new(4470, 700), 100, 0);
        assert_eq!(m.pos(), Point::new(4479, 700));
    }

    #[test]
    fn pas_de_bascule_hors_de_la_zone_commune() {
        // L'écran de droite ne couvre que y >= 0..1440, l'écran de gauche 0..1080.
        // À y = 1000 la bascule est possible ; une position candidate à y hors du voisin ne doit
        // pas faire disparaître le curseur.
        let vd = VirtualDesktop::new(vec![
            screen(0, 0, 0, 1920, 1080),
            screen(1, 1920, 600, 1280, 480), // voisin décalé vers le bas
        ]);
        // y = 200 : aucun écran à droite à cette hauteur -> on reste borné.
        let m = vd.apply(0, Point::new(1919, 200), 10, 0);
        assert_eq!(
            m,
            Motion::Stay {
                screen: 0,
                pos: Point::new(1919, 200)
            }
        );
        // y = 700 : le voisin est là -> bascule.
        let m = vd.apply(0, Point::new(1919, 700), 10, 0);
        assert_eq!(m.screen(), 1);
    }

    #[test]
    fn empilement_vertical() {
        let vd = VirtualDesktop::new(vec![
            screen(0, 0, 0, 1920, 1080),
            screen(1, 0, 1080, 1920, 1080),
        ]);
        let m = vd.apply(0, Point::new(500, 1079), 0, 3);
        assert_eq!(
            m,
            Motion::Cross {
                from: 0,
                to: 1,
                pos: Point::new(500, 1082)
            }
        );
        assert_eq!(vd.get(1).unwrap().to_local(m.pos()), Point::new(500, 2));
    }

    #[test]
    fn diagonale_depuis_le_coin() {
        // Depuis le coin bas-droit de l'écran 0, un mouvement diagonal tombe directement dans
        // l'écran 1 (plus haut que l'écran 0) : bascule à la position exacte, sans rabotage.
        let vd = side_by_side();
        let m = vd.apply(0, Point::new(1919, 1079), 20, 20);
        assert_eq!(
            m,
            Motion::Cross {
                from: 0,
                to: 1,
                pos: Point::new(1939, 1099)
            }
        );
    }

    #[test]
    fn diagonale_rase_un_coin_et_bascule() {
        // Deux écrans en diagonale, décalés : la position candidate tombe dans le vide, mais le
        // segment de déplacement traverse réellement l'écran voisin. C'est le cas que seul le
        // calcul d'intersection sait rattraper.
        let vd = VirtualDesktop::new(vec![
            screen(0, 0, 0, 1000, 1000),
            screen(1, 1000, 900, 1000, 200), // bande étroite en bas à droite
        ]);
        // Depuis (990, 950) on part fort vers la droite et vers le bas : la candidate (1990, 1250)
        // est sous la bande, mais le trajet la traverse.
        let m = vd.apply(0, Point::new(990, 950), 1000, 300);
        assert_eq!(m.screen(), 1);
        assert!(vd.get(1).unwrap().contains(m.pos()));
    }

    #[test]
    fn mouvement_nul_ne_bascule_pas() {
        let vd = side_by_side();
        let m = vd.apply(0, Point::new(1919, 500), 0, 0);
        assert_eq!(
            m,
            Motion::Stay {
                screen: 0,
                pos: Point::new(1919, 500)
            }
        );
    }

    #[test]
    fn trou_entre_ecrans_ne_perd_pas_le_curseur() {
        // 100 px de trou entre les deux écrans : impossible de traverser, mais le curseur reste
        // toujours sur un écran valide.
        let vd = VirtualDesktop::new(vec![
            screen(0, 0, 0, 1920, 1080),
            screen(1, 2020, 0, 1280, 1024),
        ]);
        let m = vd.apply(0, Point::new(1919, 500), 50, 0);
        assert_eq!(
            m,
            Motion::Stay {
                screen: 0,
                pos: Point::new(1919, 500)
            }
        );
        assert!(vd.get(m.screen()).unwrap().contains(m.pos()));
    }

    #[test]
    fn grand_saut_traverse_quand_meme() {
        // Souris rapide : un seul paquet d'événements peut dépasser l'écran voisin entier.
        let vd = side_by_side();
        let m = vd.apply(0, Point::new(1900, 500), 3000, 0);
        assert_eq!(m.screen(), 1);
        // On n'a pas quitté le bureau : la position est dans l'écran cible ou bornée à son bord.
        let target = vd.get(1).unwrap();
        assert!(target.contains(m.pos()) || target.clamp(m.pos()) == m.pos());
    }

    #[test]
    fn recherche_par_nom() {
        let vd = side_by_side();
        assert_eq!(vd.by_name("s1").map(|s| s.id), Some(1));
        assert!(vd.by_name("inconnu").is_none());
    }
}
