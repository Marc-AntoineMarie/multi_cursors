//! Injection du pointeur sous Windows, via `SendInput`.
//!
//! Comme pour Linux, l'interface système est déclarée à la main plutôt que via une crate de
//! liaisons : trois fonctions et une structure suffisent, et cela évite de suivre les versions
//! d'un gros paquet de bindings pour si peu.
//!
//! Deux particularités de la plateforme :
//!
//! * les coordonnées absolues de `SendInput` sont **normalisées sur 0..65535**, pas en pixels, et
//!   avec `MOUSEEVENTF_VIRTUALDESK` elles couvrent l'ensemble des moniteurs. On convertit donc les
//!   pixels reçus en utilisant la taille réelle du bureau virtuel ;
//! * Windows filtre l'injection vers les fenêtres plus privilégiées (UIPI) : au-dessus d'une
//!   invite UAC, rien ne se passera si le client n'est pas lui-même élevé.

use anyhow::{Context, Result};
use mc_proto::Button;

use crate::PointerSink;

type Dword = u32;
type Long = i32;
type UlongPtr = usize;

const INPUT_MOUSE: Dword = 0;

const MOUSEEVENTF_MOVE: Dword = 0x0001;
const MOUSEEVENTF_LEFTDOWN: Dword = 0x0002;
const MOUSEEVENTF_LEFTUP: Dword = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: Dword = 0x0008;
const MOUSEEVENTF_RIGHTUP: Dword = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: Dword = 0x0020;
const MOUSEEVENTF_MIDDLEUP: Dword = 0x0040;
const MOUSEEVENTF_XDOWN: Dword = 0x0080;
const MOUSEEVENTF_XUP: Dword = 0x0100;
const MOUSEEVENTF_WHEEL: Dword = 0x0800;
const MOUSEEVENTF_HWHEEL: Dword = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: Dword = 0x4000;
const MOUSEEVENTF_ABSOLUTE: Dword = 0x8000;

const XBUTTON1: Dword = 0x0001;
const XBUTTON2: Dword = 0x0002;
const WHEEL_DELTA: i32 = 120;

const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

/// Échelle des coordonnées absolues de `SendInput`.
const ABS_RANGE: i64 = 65535;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MouseInput {
    dx: Long,
    dy: Long,
    mouse_data: Dword,
    flags: Dword,
    time: Dword,
    extra_info: UlongPtr,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: Dword,
    mouse: MouseInput,
}

// Vérifié à la compilation, pas à l'exécution : `SendInput` rejette silencieusement tout tableau
// dont la taille d'élément ne correspond pas exactement à celle attendue par le système. Une
// erreur ici ne produirait aucun message, juste un curseur qui ne bouge pas — autant que la
// compilation croisée refuse de produire un binaire faux.
const _: () = assert!(std::mem::size_of::<MouseInput>() == 32);
const _: () = assert!(std::mem::size_of::<Input>() == 40);

#[link(name = "user32")]
extern "system" {
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
}

/// Taille du bureau virtuel, tous moniteurs confondus.
pub fn virtual_desktop_size() -> Result<(i32, i32)> {
    let (w, h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    anyhow::ensure!(
        w > 1 && h > 1,
        "taille du bureau virtuel invalide ({w}x{h}) : GetSystemMetrics a échoué"
    );
    Ok((w, h))
}

fn send(inputs: &[Input]) -> Result<()> {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<Input>() as i32,
        )
    };
    anyhow::ensure!(
        sent as usize == inputs.len(),
        "SendInput n'a transmis que {sent} événement(s) sur {} \
         (fenêtre plus privilégiée au premier plan ?)",
        inputs.len()
    );
    Ok(())
}

fn mouse_event(flags: Dword, dx: Long, dy: Long, data: Dword) -> Input {
    Input {
        kind: INPUT_MOUSE,
        mouse: MouseInput {
            dx,
            dy,
            mouse_data: data,
            flags,
            time: 0,
            extra_info: 0,
        },
    }
}

pub struct WindowsPointer {
    width: i32,
    height: i32,
    pressed: Vec<Button>,
}

impl WindowsPointer {
    /// `declared_*` est la géométrie annoncée au serveur. Si elle ne correspond pas au bureau
    /// réel, on garde la réalité pour l'échelle et on le signale : la disposition doit être
    /// corrigée, sinon le curseur n'entrera pas à la bonne hauteur.
    pub fn new(declared_width: i32, declared_height: i32) -> Result<Self> {
        let (width, height) =
            virtual_desktop_size().context("lecture de la taille du bureau virtuel")?;
        if declared_width != width || declared_height != height {
            tracing::warn!(
                "la disposition annonce {declared_width}x{declared_height} mais le bureau virtuel \
                 mesure {width}x{height} : corriger la configuration"
            );
        }
        tracing::info!("injection Windows prête — bureau virtuel {width}x{height}");
        Ok(Self {
            width,
            height,
            pressed: Vec::new(),
        })
    }

    /// Pixels → coordonnées normalisées 0..65535 du bureau virtuel.
    fn normalize(&self, x: i32, y: i32) -> (Long, Long) {
        let nx = i64::from(x.clamp(0, self.width - 1)) * ABS_RANGE / i64::from(self.width - 1);
        let ny = i64::from(y.clamp(0, self.height - 1)) * ABS_RANGE / i64::from(self.height - 1);
        (nx as Long, ny as Long)
    }

    fn button_flags(&self, button: Button, pressed: bool) -> (Dword, Dword) {
        match (button, pressed) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Side, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (Button::Side, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (Button::Extra, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (Button::Extra, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        }
    }
}

impl PointerSink for WindowsPointer {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (nx, ny) = self.normalize(x, y);
        send(&[mouse_event(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            nx,
            ny,
            0,
        )])
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        if pressed {
            if !self.pressed.contains(&button) {
                self.pressed.push(button);
            }
        } else {
            self.pressed.retain(|b| *b != button);
        }
        let (flags, data) = self.button_flags(button, pressed);
        send(&[mouse_event(flags, 0, 0, data)])
    }

    fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if y != 0 {
            events.push(mouse_event(
                MOUSEEVENTF_WHEEL,
                0,
                0,
                (y * WHEEL_DELTA) as Dword,
            ));
        }
        if x != 0 {
            events.push(mouse_event(
                MOUSEEVENTF_HWHEEL,
                0,
                0,
                (x * WHEEL_DELTA) as Dword,
            ));
        }
        if events.is_empty() {
            return Ok(());
        }
        send(&events)
    }

    fn release_all(&mut self) -> Result<()> {
        for button in std::mem::take(&mut self.pressed) {
            let (flags, data) = self.button_flags(button, false);
            send(&[mouse_event(flags, 0, 0, data)])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taille_abi_input() {
        // `SendInput` rejette tout tableau dont la taille d'élément ne correspond pas exactement.
        assert_eq!(std::mem::size_of::<Input>(), 40);
        assert_eq!(std::mem::size_of::<MouseInput>(), 32);
    }

    #[test]
    fn normalisation_des_bornes() {
        let pointer = WindowsPointer {
            width: 1920,
            height: 1080,
            pressed: Vec::new(),
        };
        assert_eq!(pointer.normalize(0, 0), (0, 0));
        assert_eq!(pointer.normalize(1919, 1079), (65535, 65535));
        // Hors bureau : borné, jamais d'enroulement.
        assert_eq!(pointer.normalize(-50, 99999), (0, 65535));
        let (mx, _) = pointer.normalize(960, 0);
        assert!((32700..32800).contains(&mx), "milieu attendu, obtenu {mx}");
    }
}
