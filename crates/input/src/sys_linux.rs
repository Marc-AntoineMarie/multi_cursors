//! Liaison directe avec les ABI noyau `evdev` et `uinput`.
//!
//! Pourquoi écrire ces ioctls à la main plutôt qu'utiliser la crate `evdev` : il nous faut
//! `UI_SET_PROPBIT` pour déclarer `INPUT_PROP_POINTER` sur notre périphérique virtuel, et la crate
//! ne l'expose pas. Or ce bit est central — c'est lui qui fait classer notre pointeur absolu comme
//! une souris par udev/libinput (branche `BTN_MOUSE` de `input_id`) au lieu d'un écran tactile ou
//! d'une tablette. Sans lui, le compositeur ne place pas le curseur là où on le demande.
//!
//! Hypothèse de plateforme : Linux 64 bits (x86_64, aarch64). `input_event` y mesure 24 octets et
//! les masques de capacités exposés par sysfs sont découpés en mots de 64 bits.

#![allow(clippy::unusual_byte_groupings)]

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

// --- Types d'événements (linux/input-event-codes.h) ---
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0x00;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_TOOL_FINGER: u16 = 0x145;
pub const BTN_TOUCH: u16 = 0x14a;

pub const KEY_A: u16 = 0x1e;

pub const INPUT_PROP_POINTER: u16 = 0x00;
pub const BUS_VIRTUAL: u16 = 0x06;

// --- Structures ABI ---

/// `struct input_event` : 24 octets sur Linux 64 bits.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub time: libc::timeval,
    /// `type` en C ; renommé car c'est un mot-clé en Rust.
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    pub fn new(kind: u16, code: u16, value: i32) -> Self {
        Self {
            // Le noyau réécrit l'horodatage à l'écriture : inutile de le renseigner.
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            kind,
            code,
            value,
        }
    }

    pub fn syn() -> Self {
        Self::new(EV_SYN, SYN_REPORT, 0)
    }
}

// Mêmes garde-fous que côté Windows : une structure mal dimensionnée produirait des lectures
// décalées de /dev/input ou des ioctl refusés, sans message clair.
const _: () = assert!(std::mem::size_of::<InputEvent>() == 24);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    /// Unités par millimètre pour une surface tactile, 0 si inconnue.
    pub resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

// --- Numéros d'ioctl (_IO / _IOW de linux/ioctl.h) ---

const fn io(ty: u64, nr: u64) -> u64 {
    (ty << 8) | nr
}
const fn iow(ty: u64, nr: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | (ty << 8) | nr
}

const UI: u64 = b'U' as u64;
const EV: u64 = b'E' as u64;

const UI_DEV_CREATE: u64 = io(UI, 1);
const UI_DEV_DESTROY: u64 = io(UI, 2);
const UI_DEV_SETUP: u64 = iow(UI, 3, std::mem::size_of::<UinputSetup>() as u64);
const UI_ABS_SETUP: u64 = iow(UI, 4, std::mem::size_of::<UinputAbsSetup>() as u64);
const UI_SET_EVBIT: u64 = iow(UI, 100, 4);
const UI_SET_KEYBIT: u64 = iow(UI, 101, 4);
const UI_SET_RELBIT: u64 = iow(UI, 102, 4);
const UI_SET_ABSBIT: u64 = iow(UI, 103, 4);
const UI_SET_PROPBIT: u64 = iow(UI, 110, 4);

const _: () = assert!(std::mem::size_of::<UinputSetup>() == 92);
const _: () = assert!(std::mem::size_of::<UinputAbsSetup>() == 28);

const EVIOCGRAB: u64 = iow(EV, 0x90, 4);

const fn ior(ty: u64, nr: u64, size: u64) -> u64 {
    (2 << 30) | (size << 16) | (ty << 8) | nr
}

/// `EVIOCGABS(axe)` : bornes et résolution d'un axe absolu.
const fn eviocgabs(axis: u16) -> u64 {
    ior(
        EV,
        0x40 + axis as u64,
        std::mem::size_of::<InputAbsinfo>() as u64,
    )
}

fn check(ret: libc::c_int, what: &str) -> io::Result<()> {
    if ret < 0 {
        let err = io::Error::last_os_error();
        Err(io::Error::new(
            err.kind(),
            format!("{what} a échoué : {err}"),
        ))
    } else {
        Ok(())
    }
}

unsafe fn set_bit(fd: libc::c_int, request: u64, bit: u16, what: &str) -> io::Result<()> {
    check(
        libc::ioctl(fd, request as libc::c_ulong, libc::c_int::from(bit as i32)),
        what,
    )
}

// --- Périphériques réels ---

pub fn open_device(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // Non bloquant : la boucle de capture fait un `poll` puis vide la file jusqu'à EAGAIN.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) })
}

/// Prise exclusive du périphérique : le compositeur ne reçoit plus rien de lui.
///
/// Le noyau relâche la prise dès que le descripteur est fermé — y compris si le processus meurt.
/// C'est notre principal filet de sécurité : un crash rend la souris au système.
pub fn grab(fd: &OwnedFd, grabbed: bool) -> io::Result<()> {
    let value: libc::c_int = i32::from(grabbed);
    check(
        unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGRAB as libc::c_ulong, value) },
        if grabbed { "EVIOCGRAB" } else { "EVIOCGRAB(0)" },
    )
}

/// Interroge les bornes d'un axe absolu (nécessaire pour mettre un pavé tactile à l'échelle).
pub fn get_absinfo(fd: &OwnedFd, axis: u16) -> io::Result<InputAbsinfo> {
    let mut info = InputAbsinfo::default();
    let ret = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            eviocgabs(axis) as libc::c_ulong,
            &mut info as *mut InputAbsinfo,
        )
    };
    check(ret, "EVIOCGABS")?;
    Ok(info)
}

/// Vide la file d'événements d'un périphérique et renvoie les événements lus.
pub fn read_events(fd: &OwnedFd, buf: &mut Vec<InputEvent>) -> io::Result<()> {
    let mut raw = [0u8; InputEvent::SIZE * 64];
    loop {
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                raw.as_mut_ptr().cast::<libc::c_void>(),
                raw.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(()),
                _ => Err(err),
            };
        }
        if n == 0 {
            return Ok(());
        }
        let n = n as usize;
        for chunk in raw[..n].chunks_exact(InputEvent::SIZE) {
            // Les octets viennent du noyau dans l'ordre natif : lecture directe.
            let ev = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
            buf.push(ev);
        }
        if n < raw.len() {
            return Ok(());
        }
    }
}

// --- Découverte des périphériques via sysfs ---

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    /// Souris classique : deltas relatifs + bouton gauche.
    pub relative_pointer: bool,
    /// Pavé tactile : axes absolus multipoints. Non exploitable en V1 (voir README).
    pub touchpad: bool,
    pub keyboard: bool,
}

/// Teste un bit dans un masque de capacités sysfs.
///
/// Format : des mots hexadécimaux séparés par des espaces, du plus significatif au moins
/// significatif, en morceaux de `BITS_PER_LONG` (64 sur nos cibles).
fn has_bit(mask: &str, bit: u16) -> bool {
    let words: Vec<u64> = mask
        .split_whitespace()
        .rev()
        .map(|w| u64::from_str_radix(w, 16).unwrap_or(0))
        .collect();
    let (word, offset) = (usize::from(bit) / 64, usize::from(bit) % 64);
    words.get(word).is_some_and(|w| (w >> offset) & 1 == 1)
}

fn sysfs_attr(event_name: &str, attr: &str) -> String {
    let path = format!("/sys/class/input/{event_name}/device/{attr}");
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Énumère les périphériques d'entrée et les classe.
pub fn list_devices() -> io::Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let entries = fs::read_dir("/dev/input")?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !file.starts_with("event") {
            continue;
        }

        let caps_ev = sysfs_attr(file, "capabilities/ev");
        let caps_rel = sysfs_attr(file, "capabilities/rel");
        let caps_key = sysfs_attr(file, "capabilities/key");
        let caps_abs = sysfs_attr(file, "capabilities/abs");

        let has_rel_xy = has_bit(&caps_ev, EV_REL) && has_bit(&caps_rel, REL_X) && has_bit(&caps_rel, REL_Y);
        let has_left = has_bit(&caps_key, BTN_LEFT);
        let has_abs_xy = has_bit(&caps_ev, EV_ABS) && has_bit(&caps_abs, ABS_X) && has_bit(&caps_abs, ABS_Y);

        out.push(DeviceInfo {
            name: sysfs_attr(file, "name"),
            relative_pointer: has_rel_xy && has_left,
            touchpad: has_abs_xy
                && (has_bit(&caps_key, BTN_TOOL_FINGER) || has_bit(&caps_key, BTN_TOUCH))
                && !has_bit(&caps_key, BTN_TOOL_PEN),
            keyboard: has_bit(&caps_ev, EV_KEY) && has_bit(&caps_key, KEY_A),
            path,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Taille du bureau d'après sysfs DRM — meilleur effort.
///
/// On lit le mode préféré du premier connecteur branché, dans l'ordre alphabétique. Cela suffit
/// pour peupler une configuration neuve sur une machine à un seul écran. Avec plusieurs moniteurs,
/// sysfs ne dit rien de leur disposition : la valeur doit alors être corrigée à la main (ou, à
/// l'étape 2, poussée par l'interface qui connaît la vraie géométrie logique).
pub fn drm_geometry() -> Option<(i32, i32)> {
    let mut candidates: Vec<(String, i32, i32)> = Vec::new();
    for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Les connecteurs portent un nom de la forme « card0-eDP-1 » ; on ignore les cartes.
        if !name.contains('-') {
            continue;
        }
        if fs::read_to_string(path.join("status")).map(|s| s.trim() != "connected").unwrap_or(true) {
            continue;
        }
        let modes = fs::read_to_string(path.join("modes")).unwrap_or_default();
        let Some((w, h)) = modes.lines().next().and_then(|l| l.trim().split_once('x')) else {
            continue;
        };
        if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
            candidates.push((name, w, h));
        }
    }
    candidates.sort();
    candidates.first().map(|(_, w, h)| (*w, *h))
}

// --- Périphérique virtuel uinput ---

pub struct UinputDevice {
    fd: OwnedFd,
}

impl UinputDevice {
    /// Crée un pointeur **absolu** : `INPUT_PROP_POINTER` + `ABS_X`/`ABS_Y` bornés à la taille du
    /// bureau, boutons et molette.
    ///
    /// Les bornes absolues valent exactement `largeur - 1` et `hauteur - 1` : libinput normalise
    /// l'axe sur `0..1` puis le compositeur le remappe sur toute la zone logique, donc la
    /// correspondance pixel ↔ valeur est l'identité. Aucun arrondi, aucune dérive, et le curseur
    /// atterrit au pixel demandé sous Wayland comme sous X11.
    pub fn new_absolute_pointer(name: &str, width: i32, height: i32) -> io::Result<Self> {
        let c = CString::new("/dev/uinput").unwrap();
        let raw = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "ouverture de /dev/uinput impossible : {err}. \
                     Vérifier la règle udev et l'appartenance au groupe « input » (voir `mcd check`)"
                ),
            ));
        }
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        let n = fd.as_raw_fd();

        unsafe {
            set_bit(n, UI_SET_PROPBIT, INPUT_PROP_POINTER, "UI_SET_PROPBIT")?;

            set_bit(n, UI_SET_EVBIT, EV_KEY, "UI_SET_EVBIT(EV_KEY)")?;
            for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                set_bit(n, UI_SET_KEYBIT, btn, "UI_SET_KEYBIT")?;
            }

            set_bit(n, UI_SET_EVBIT, EV_REL, "UI_SET_EVBIT(EV_REL)")?;
            set_bit(n, UI_SET_RELBIT, REL_WHEEL, "UI_SET_RELBIT(REL_WHEEL)")?;
            set_bit(n, UI_SET_RELBIT, REL_HWHEEL, "UI_SET_RELBIT(REL_HWHEEL)")?;

            set_bit(n, UI_SET_EVBIT, EV_ABS, "UI_SET_EVBIT(EV_ABS)")?;
            for (code, max) in [(ABS_X, width - 1), (ABS_Y, height - 1)] {
                set_bit(n, UI_SET_ABSBIT, code, "UI_SET_ABSBIT")?;
                let setup = UinputAbsSetup {
                    code,
                    absinfo: InputAbsinfo {
                        minimum: 0,
                        maximum: max.max(1),
                        ..InputAbsinfo::default()
                    },
                };
                check(
                    libc::ioctl(n, UI_ABS_SETUP as libc::c_ulong, &setup),
                    "UI_ABS_SETUP",
                )?;
            }

            let mut name_buf = [0u8; 80];
            let bytes = name.as_bytes();
            let len = bytes.len().min(name_buf.len() - 1);
            name_buf[..len].copy_from_slice(&bytes[..len]);
            let setup = UinputSetup {
                id: InputId {
                    bustype: BUS_VIRTUAL,
                    vendor: 0x1d6b,
                    product: 0x0001,
                    version: 1,
                },
                name: name_buf,
                ff_effects_max: 0,
            };
            check(
                libc::ioctl(n, UI_DEV_SETUP as libc::c_ulong, &setup),
                "UI_DEV_SETUP",
            )?;
            check(libc::ioctl(n, UI_DEV_CREATE as libc::c_ulong), "UI_DEV_CREATE")?;
        }

        Ok(Self { fd })
    }

    /// Écrit un lot d'événements suivi d'un `SYN_REPORT`.
    ///
    /// Le `SYN_REPORT` final est ce qui rend le lot atomique pour le compositeur : X et Y d'un même
    /// déplacement arrivent ensemble, donc pas de curseur en escalier.
    pub fn emit(&self, events: &[InputEvent]) -> io::Result<()> {
        let mut batch: Vec<InputEvent> = Vec::with_capacity(events.len() + 1);
        batch.extend_from_slice(events);
        batch.push(InputEvent::syn());

        let bytes = unsafe {
            std::slice::from_raw_parts(
                batch.as_ptr().cast::<u8>(),
                std::mem::size_of_val(batch.as_slice()),
            )
        };
        let mut written = 0usize;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    bytes[written..].as_ptr().cast::<libc::c_void>(),
                    bytes.len() - written,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            written += n as usize;
        }
        Ok(())
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY as libc::c_ulong);
        }
    }
}

/// Attend que l'un des descripteurs ait des données. Renvoie les index prêts.
pub fn poll_readable(fds: &[libc::c_int], timeout_ms: i32, ready: &mut Vec<usize>) -> io::Result<()> {
    ready.clear();
    if fds.is_empty() {
        // Rien à surveiller : on attend quand même pour ne pas tourner à vide.
        unsafe {
            libc::poll(std::ptr::null_mut(), 0, timeout_ms);
        }
        return Ok(());
    }

    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let n = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(err);
    }
    for (i, p) in pollfds.iter().enumerate() {
        if p.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            ready.push(i);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taille_abi_input_event() {
        // Si cette assertion casse, c'est que la cible n'est pas un Linux 64 bits classique et que
        // la lecture brute de /dev/input serait décalée.
        assert_eq!(InputEvent::SIZE, 24);
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
    }

    #[test]
    fn numeros_ioctl_connus() {
        // Valeurs vérifiables à la main depuis linux/uinput.h et linux/input.h.
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_PROPBIT, 0x4004_556e);
        assert_eq!(EVIOCGRAB, 0x4004_4590);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(eviocgabs(ABS_X), 0x8018_4540);
    }

    #[test]
    fn lecture_masque_capacites() {
        // Masque typique d'une souris : REL_X|REL_Y|REL_WHEEL = bits 0, 1 et 8.
        assert!(has_bit("103", REL_X));
        assert!(has_bit("103", REL_Y));
        assert!(has_bit("103", REL_WHEEL));
        assert!(!has_bit("103", REL_HWHEEL));

        // BTN_LEFT = 0x110 = bit 272 : deuxième mot (272/64 = 4) depuis la fin.
        let key_mask = "10000 0 0 0 0";
        assert!(has_bit(key_mask, BTN_LEFT));
        assert!(!has_bit(key_mask, KEY_A));

        assert!(!has_bit("", REL_X));
    }
}
