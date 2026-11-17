//! Capture exclusive des périphériques de pointage sous Linux.
//!
//! Principe : on prend les périphériques réels en exclusivité (`EVIOCGRAB`) et on devient la seule
//! source de vérité sur la position du curseur. Le compositeur ne voit plus que notre pointeur
//! virtuel absolu (voir `sink_linux`). C'est ce qui rend la bascule entre écrans identique sous
//! Wayland et X11, sans portail ni autorisation.
//!
//! Deux familles de périphériques sont lues :
//!
//! * **souris relatives** — `REL_X`/`REL_Y`, traitement direct ;
//! * **pavés tactiles** — axes absolus multipoints, convertis en déplacements. Indispensable sur un
//!   portable : un pavé tactile « Precision » moderne n'émet rien sur son nœud souris, tout passe
//!   par le nœud multipoint. Sans cette conversion, l'application serait inutilisable sans souris
//!   USB branchée.
//!
//! La conversion multipoint reste volontairement simple : déplacement à un doigt, clic physique,
//! tape pour cliquer, défilement à deux doigts. Les gestes riches (trois doigts, pincement,
//! rejet de paume fin) supposent de passer par libinput, prévu plus tard.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mc_proto::Button;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::sys_linux as sys;
use crate::{RawEvent, Tuning};

/// Préfixe de nos propres périphériques virtuels : on ne les capture jamais, sinon le pointeur
/// s'auto-alimenterait en boucle.
pub const VIRTUAL_PREFIX: &str = "MultiCursos";

const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT_MS: i32 = 200;
const MAX_SLOTS: usize = 10;

/// Durée et distance maximales d'un contact pour être interprété comme une tape.
const TAP_MAX_DURATION: Duration = Duration::from_millis(180);
const TAP_MAX_TRAVEL_MM: f64 = 2.0;

/// Commandes partagées avec le fil de capture.
#[derive(Debug)]
pub struct CaptureControl {
    grab: AtomicBool,
    stop: AtomicBool,
}

impl CaptureControl {
    /// Prise ou relâchement des périphériques. Le relâchement est le « bouton panique » : le
    /// système retrouve immédiatement sa souris, sans arrêter le démon.
    pub fn set_grabbed(&self, grabbed: bool) {
        self.grab.store(grabbed, Ordering::SeqCst);
    }
    pub fn is_grabbed(&self) -> bool {
        self.grab.load(Ordering::SeqCst)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub struct Capture {
    rx: UnboundedReceiver<RawEvent>,
    control: Arc<CaptureControl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Démarre la capture dans un fil dédié.
    ///
    /// Un fil système plutôt qu'une tâche asynchrone : les lectures `evdev` sont bloquantes et
    /// doivent être servies sans passer par l'ordonnanceur d'un exécuteur, c'est quelques
    /// centaines de microsecondes de latence en moins sur chaque déplacement.
    pub fn start(tuning: Tuning) -> Result<Self> {
        let found = list_capturable()?;
        anyhow::ensure!(
            !found.is_empty(),
            "aucun périphérique de pointage détecté dans /dev/input"
        );

        // Les périphériques sont ouverts ici, avant de démarrer le fil : un démarrage qui
        // « réussit » puis répète indéfiniment une erreur de droits est pire qu'un refus net.
        let mut devices = Vec::new();
        let mut failures = Vec::new();
        for info in &found {
            match Device::open(info, &tuning) {
                Ok(device) => {
                    tracing::info!("capture de « {} » ({})", info.name, info.path.display());
                    devices.push(device);
                }
                Err(err) => failures.push(format!("{} : {err:#}", info.path.display())),
            }
        }
        if devices.is_empty() {
            anyhow::bail!(
                "aucun périphérique de pointage n'a pu être ouvert :\n  {}\n\
                 Il s'agit presque toujours d'un manque de droits — lancer `mcd check`",
                failures.join("\n  ")
            );
        }
        for failure in &failures {
            tracing::warn!("périphérique ignoré — {failure}");
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let control = Arc::new(CaptureControl {
            grab: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });

        let thread_control = Arc::clone(&control);
        let thread = std::thread::Builder::new()
            .name("mc-capture".into())
            .spawn(move || {
                if let Err(err) = capture_loop(devices, &thread_control, &tuning, &tx) {
                    tracing::error!("fil de capture arrêté : {err:#}");
                }
            })
            .context("démarrage du fil de capture")?;

        Ok(Self {
            rx,
            control,
            thread: Some(thread),
        })
    }

    pub async fn recv(&mut self) -> Option<RawEvent> {
        self.rx.recv().await
    }

    pub fn control(&self) -> Arc<CaptureControl> {
        Arc::clone(&self.control)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.control.stop();
        if let Some(thread) = self.thread.take() {
            // On attend la fin du fil : c'est lui qui détient les descripteurs, et leur fermeture
            // est ce qui relâche la prise exclusive. Sans cette attente, le processus pourrait
            // rendre la main avant que le système ait récupéré la souris.
            let _ = thread.join();
        }
    }
}

/// Périphériques que l'on sait exploiter, hors périphériques virtuels à nous.
pub fn list_capturable() -> Result<Vec<sys::DeviceInfo>> {
    Ok(sys::list_devices()
        .context("énumération de /dev/input")?
        .into_iter()
        .filter(|d| (d.relative_pointer || d.touchpad) && !d.name.starts_with(VIRTUAL_PREFIX))
        .collect())
}

// --- Lecteurs ---

/// Accumulation d'un déplacement sur une trame (entre deux `SYN_REPORT`).
#[derive(Default)]
struct Frame {
    dx: f64,
    dy: f64,
    scroll_x: i32,
    scroll_y: i32,
    buttons: Vec<(Button, bool)>,
}

impl Frame {
    fn is_empty(&self) -> bool {
        self.dx == 0.0
            && self.dy == 0.0
            && self.scroll_x == 0
            && self.scroll_y == 0
            && self.buttons.is_empty()
    }
}

fn button_from_code(code: u16) -> Option<Button> {
    match code {
        sys::BTN_LEFT => Some(Button::Left),
        sys::BTN_RIGHT => Some(Button::Right),
        sys::BTN_MIDDLE => Some(Button::Middle),
        sys::BTN_SIDE => Some(Button::Side),
        sys::BTN_EXTRA => Some(Button::Extra),
        _ => None,
    }
}

/// État d'un pavé tactile multipoint.
struct Touchpad {
    /// Pixels par unité de pavé, par axe.
    scale_x: f64,
    scale_y: f64,
    /// Unités par millimètre, pour les seuils exprimés en millimètres.
    units_per_mm: f64,
    slot: usize,
    fingers: [bool; MAX_SLOTS],
    finger_count: usize,
    position: Option<(i32, i32)>,
    last: Option<(i32, i32)>,
    contact_started: Option<Instant>,
    travel_units: f64,
    scroll_accumulator: f64,
}

impl Touchpad {
    fn new(fd: &OwnedFd, tuning: &Tuning) -> Result<Self> {
        let x = sys::get_absinfo(fd, sys::ABS_X).context("bornes de l'axe X du pavé tactile")?;
        let y = sys::get_absinfo(fd, sys::ABS_Y).context("bornes de l'axe Y du pavé tactile")?;

        // `resolution` vaut normalement le nombre d'unités par millimètre. Quand le pilote ne la
        // renseigne pas (0), on suppose un pavé d'environ 100 mm de large : approximatif mais ça
        // évite une sensibilité absurde.
        let res_x = if x.resolution > 0 {
            f64::from(x.resolution)
        } else {
            f64::from(x.maximum - x.minimum).max(1.0) / 100.0
        };
        let res_y = if y.resolution > 0 {
            f64::from(y.resolution)
        } else {
            f64::from(y.maximum - y.minimum).max(1.0) / 100.0
        };

        Ok(Self {
            scale_x: tuning.touchpad_pixels_per_mm / res_x,
            scale_y: tuning.touchpad_pixels_per_mm / res_y,
            units_per_mm: res_x,
            slot: 0,
            fingers: [false; MAX_SLOTS],
            finger_count: 0,
            position: None,
            last: None,
            contact_started: None,
            travel_units: 0.0,
            scroll_accumulator: 0.0,
        })
    }

    fn handle(&mut self, ev: &sys::InputEvent, frame: &mut Frame, tuning: &Tuning) {
        match ev.kind {
            sys::EV_ABS => match ev.code {
                sys::ABS_X => {
                    let (_, y) = self.position.unwrap_or((ev.value, 0));
                    self.position = Some((ev.value, y));
                }
                sys::ABS_Y => {
                    let (x, _) = self.position.unwrap_or((0, ev.value));
                    self.position = Some((x, ev.value));
                }
                sys::ABS_MT_SLOT => {
                    self.slot = (ev.value.max(0) as usize).min(MAX_SLOTS - 1);
                }
                sys::ABS_MT_TRACKING_ID => {
                    let down = ev.value >= 0;
                    if self.fingers[self.slot] != down {
                        self.fingers[self.slot] = down;
                        self.finger_count = self.fingers.iter().filter(|f| **f).count();
                        // Le nombre de doigts change : la position du contact principal saute
                        // (autre doigt devenu primaire). On repart de la nouvelle position pour ne
                        // pas transformer ce saut en déplacement du curseur.
                        self.last = None;
                        if self.finger_count == 1 && down {
                            self.contact_started = Some(Instant::now());
                            self.travel_units = 0.0;
                        }
                        if self.finger_count == 0 {
                            self.finish_contact(frame, tuning);
                        }
                    }
                }
                _ => {}
            },
            sys::EV_KEY => {
                if let Some(button) = button_from_code(ev.code) {
                    // Clic physique du pavé : transmis tel quel, et il annule la tape en cours.
                    self.contact_started = None;
                    frame.buttons.push((button, ev.value != 0));
                }
            }
            _ => {}
        }
    }

    /// Fin de trame : convertit la nouvelle position du contact en déplacement ou en défilement.
    fn flush(&mut self, frame: &mut Frame, tuning: &Tuning) {
        let Some((x, y)) = self.position else {
            return;
        };
        if self.finger_count == 0 {
            self.last = None;
            return;
        }

        let Some((lx, ly)) = self.last else {
            self.last = Some((x, y));
            return;
        };
        let (dx_units, dy_units) = (f64::from(x - lx), f64::from(y - ly));
        self.last = Some((x, y));
        self.travel_units += dx_units.hypot(dy_units);

        match self.finger_count {
            1 => {
                frame.dx += dx_units * self.scale_x;
                frame.dy += dy_units * self.scale_y;
            }
            2 => {
                // Défilement à deux doigts. Le signe est inversé par rapport au déplacement du
                // doigt : glisser vers le bas fait descendre le contenu, comme partout ailleurs.
                let mm_per_notch = tuning.touchpad_mm_per_scroll.max(0.5);
                self.scroll_accumulator += -dy_units / (self.units_per_mm * mm_per_notch);
                let notches = self.scroll_accumulator.trunc();
                if notches != 0.0 {
                    frame.scroll_y += notches as i32;
                    self.scroll_accumulator -= notches;
                }
            }
            _ => {}
        }
    }

    /// Dernier doigt levé : éventuelle tape pour cliquer.
    fn finish_contact(&mut self, frame: &mut Frame, tuning: &Tuning) {
        let Some(started) = self.contact_started.take() else {
            return;
        };
        if !tuning.tap_to_click {
            return;
        }
        let travel_mm = self.travel_units / self.units_per_mm.max(0.1);
        if started.elapsed() <= TAP_MAX_DURATION && travel_mm <= TAP_MAX_TRAVEL_MM {
            frame.buttons.push((Button::Left, true));
            frame.buttons.push((Button::Left, false));
        }
    }
}

enum Reader {
    Relative,
    Touchpad(Box<Touchpad>),
}

struct Device {
    path: PathBuf,
    name: String,
    fd: OwnedFd,
    grabbed: bool,
    reader: Reader,
    frame: Frame,
}

impl Device {
    fn open(info: &sys::DeviceInfo, tuning: &Tuning) -> Result<Self> {
        let fd = sys::open_device(&info.path)
            .with_context(|| format!("ouverture de {}", info.path.display()))?;
        // Un périphérique qui est à la fois relatif et tactile (rare) est traité en relatif :
        // c'est le chemin le plus fiable.
        let reader = if info.relative_pointer {
            Reader::Relative
        } else {
            Reader::Touchpad(Box::new(Touchpad::new(&fd, tuning)?))
        };
        Ok(Self {
            path: info.path.clone(),
            name: info.name.clone(),
            fd,
            grabbed: false,
            reader,
            frame: Frame::default(),
        })
    }

    fn set_grabbed(&mut self, want: bool) {
        if self.grabbed == want {
            return;
        }
        match sys::grab(&self.fd, want) {
            Ok(()) => {
                self.grabbed = want;
                tracing::debug!(
                    "« {} » {}",
                    self.name,
                    if want { "capturé" } else { "relâché" }
                );
            }
            Err(err) => {
                // Cas courant : un autre programme (Deskflow, un autre MultiCursos) détient déjà
                // la prise exclusive.
                tracing::warn!(
                    "impossible de {} « {} » : {err}",
                    if want { "capturer" } else { "relâcher" },
                    self.name
                );
            }
        }
    }

    /// Traite les événements disponibles et renvoie les trames complètes.
    fn drain(&mut self, tuning: &Tuning, out: &mut Vec<Frame>) -> std::io::Result<()> {
        let mut events = Vec::new();
        sys::read_events(&self.fd, &mut events)?;

        for ev in &events {
            if ev.kind == sys::EV_SYN && ev.code == sys::SYN_REPORT {
                if let Reader::Touchpad(pad) = &mut self.reader {
                    pad.flush(&mut self.frame, tuning);
                }
                let frame = std::mem::take(&mut self.frame);
                if !frame.is_empty() {
                    out.push(frame);
                }
                continue;
            }

            match &mut self.reader {
                Reader::Relative => match ev.kind {
                    sys::EV_REL => match ev.code {
                        sys::REL_X => self.frame.dx += f64::from(ev.value),
                        sys::REL_Y => self.frame.dy += f64::from(ev.value),
                        sys::REL_WHEEL => self.frame.scroll_y += ev.value,
                        sys::REL_HWHEEL => self.frame.scroll_x += ev.value,
                        _ => {}
                    },
                    sys::EV_KEY => {
                        if let Some(button) = button_from_code(ev.code) {
                            self.frame.buttons.push((button, ev.value != 0));
                        }
                    }
                    _ => {}
                },
                Reader::Touchpad(pad) => pad.handle(ev, &mut self.frame, tuning),
            }
        }
        Ok(())
    }
}

/// Reste fractionnaire des déplacements.
///
/// Avec une sensibilité à 0,4 par exemple, chaque petit déplacement arrondirait à zéro et la souris
/// paraîtrait collée. On conserve donc le reste d'une trame à l'autre.
#[derive(Default)]
struct Remainder {
    x: f64,
    y: f64,
}

impl Remainder {
    fn take(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let (ix, iy) = (self.x.trunc(), self.y.trunc());
        self.x -= ix;
        self.y -= iy;
        (ix as i32, iy as i32)
    }
}

fn capture_loop(
    mut devices: Vec<Device>,
    control: &CaptureControl,
    tuning: &Tuning,
    tx: &mpsc::UnboundedSender<RawEvent>,
) -> Result<()> {
    let mut remainder = Remainder::default();
    let mut ready = Vec::new();
    let mut frames = Vec::new();
    // Les périphériques fournis au démarrage sont déjà ouverts : le premier rebalayage sert
    // uniquement à détecter les branchements ultérieurs.
    let mut last_scan = Instant::now();
    // Chemins déjà signalés comme inutilisables, pour ne pas répéter le même avertissement à
    // chaque rebalayage.
    let mut failed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    loop {
        if control.stop.load(Ordering::SeqCst) {
            // Les descripteurs sont fermés en sortant : le noyau relâche la prise exclusive.
            tracing::info!("capture arrêtée, périphériques relâchés");
            return Ok(());
        }

        // Branchement à chaud : on rebalaie périodiquement plutôt que de dépendre de udev.
        if last_scan.elapsed() >= RESCAN_INTERVAL {
            last_scan = Instant::now();
            match list_capturable() {
                Ok(found) => {
                    devices.retain(|d| {
                        let still_there = found.iter().any(|f| f.path == d.path);
                        if !still_there {
                            tracing::info!("« {} » débranché", d.name);
                        }
                        still_there
                    });
                    failed.retain(|path| found.iter().any(|f| &f.path == path));

                    for info in &found {
                        if devices.iter().any(|d| d.path == info.path) {
                            continue;
                        }
                        match Device::open(info, tuning) {
                            Ok(device) => {
                                tracing::info!("« {} » branché", info.name);
                                failed.remove(&info.path);
                                devices.push(device);
                            }
                            Err(err) => {
                                // Signalé une seule fois par périphérique : sinon le journal se
                                // remplit du même message toutes les deux secondes.
                                if failed.insert(info.path.clone()) {
                                    tracing::warn!("{} inutilisable : {err:#}", info.path.display());
                                }
                            }
                        }
                    }
                }
                Err(err) => tracing::warn!("rebalayage des périphériques impossible : {err:#}"),
            }
        }

        let want_grab = control.is_grabbed();
        for device in &mut devices {
            device.set_grabbed(want_grab);
        }

        let fds: Vec<libc::c_int> = devices.iter().map(|d| d.fd.as_raw_fd()).collect();
        sys::poll_readable(&fds, POLL_TIMEOUT_MS, &mut ready).context("poll des périphériques")?;

        let mut dead: Vec<usize> = Vec::new();
        for &index in &ready {
            frames.clear();
            let device = &mut devices[index];
            if let Err(err) = device.drain(tuning, &mut frames) {
                tracing::warn!("lecture de « {} » impossible : {err}", device.name);
                dead.push(index);
                continue;
            }

            // Hors capture, on lit quand même pour vider la file du périphérique, mais on ne
            // transmet rien : le compositeur gère la souris localement pendant ce temps.
            if !want_grab {
                continue;
            }

            for frame in frames.drain(..) {
                let (dx, dy) = remainder.take(
                    frame.dx * tuning.sensitivity,
                    frame.dy * tuning.sensitivity,
                );
                if dx != 0 || dy != 0 {
                    if tx.send(RawEvent::Motion { dx, dy }).is_err() {
                        return Ok(()); // moteur arrêté
                    }
                }
                if frame.scroll_x != 0 || frame.scroll_y != 0 {
                    let _ = tx.send(RawEvent::Scroll {
                        x: frame.scroll_x,
                        y: frame.scroll_y,
                    });
                }
                for (button, pressed) in frame.buttons {
                    let _ = tx.send(RawEvent::Button { button, pressed });
                }
            }
        }

        for index in dead.into_iter().rev() {
            devices.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_reste_fractionnaire_ne_perd_pas_de_mouvement() {
        let mut r = Remainder::default();
        // Sensibilité faible : chaque trame arrondirait à zéro sans accumulation.
        let mut total = 0;
        for _ in 0..10 {
            let (dx, _) = r.take(0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, 4, "4 pixels attendus après 10 trames à 0,4");
    }

    #[test]
    fn le_reste_fonctionne_en_negatif() {
        let mut r = Remainder::default();
        let mut total = 0;
        for _ in 0..10 {
            let (dx, _) = r.take(-0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, -4);
    }

    #[test]
    fn correspondance_des_boutons() {
        assert_eq!(button_from_code(sys::BTN_LEFT), Some(Button::Left));
        assert_eq!(button_from_code(sys::BTN_EXTRA), Some(Button::Extra));
        assert_eq!(button_from_code(0x999), None);
    }
}
