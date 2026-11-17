//! Injection du pointeur sous Linux, via un périphérique virtuel uinput absolu.

use anyhow::{Context, Result};
use mc_proto::Button;

use crate::sys_linux as sys;
use crate::PointerSink;

fn button_code(button: Button) -> u16 {
    match button {
        Button::Left => sys::BTN_LEFT,
        Button::Right => sys::BTN_RIGHT,
        Button::Middle => sys::BTN_MIDDLE,
        Button::Side => sys::BTN_SIDE,
        Button::Extra => sys::BTN_EXTRA,
    }
}

pub struct UinputPointer {
    device: sys::UinputDevice,
    width: i32,
    height: i32,
    /// Boutons actuellement enfoncés, pour pouvoir tout relâcher d'un coup.
    ///
    /// Sans ce suivi, quitter un écran avec un bouton enfoncé laisserait un clic bloqué sur la
    /// machine qu'on vient de quitter — une sélection qui s'étend à l'infini, par exemple.
    pressed: Vec<Button>,
}

impl UinputPointer {
    pub fn new(name: &str, width: i32, height: i32) -> Result<Self> {
        anyhow::ensure!(
            width > 1 && height > 1,
            "géométrie invalide pour le pointeur virtuel : {width}x{height}"
        );
        let device = sys::UinputDevice::new_absolute_pointer(name, width, height)
            .context("création du pointeur virtuel uinput")?;
        tracing::info!(
            "pointeur virtuel « {name} » créé ({width}x{height}) — le compositeur peut mettre une \
             seconde à le prendre en compte"
        );
        Ok(Self {
            device,
            width,
            height,
            pressed: Vec::new(),
        })
    }

    pub fn geometry(&self) -> (i32, i32) {
        (self.width, self.height)
    }
}

impl PointerSink for UinputPointer {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let x = x.clamp(0, self.width - 1);
        let y = y.clamp(0, self.height - 1);
        self.device
            .emit(&[
                sys::InputEvent::new(sys::EV_ABS, sys::ABS_X, x),
                sys::InputEvent::new(sys::EV_ABS, sys::ABS_Y, y),
            ])
            .context("émission d'un déplacement")?;
        Ok(())
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        if pressed {
            if !self.pressed.contains(&button) {
                self.pressed.push(button);
            }
        } else {
            self.pressed.retain(|b| *b != button);
        }
        self.device
            .emit(&[sys::InputEvent::new(
                sys::EV_KEY,
                button_code(button),
                i32::from(pressed),
            )])
            .context("émission d'un clic")?;
        Ok(())
    }

    fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
        let mut events = Vec::with_capacity(2);
        if y != 0 {
            events.push(sys::InputEvent::new(sys::EV_REL, sys::REL_WHEEL, y));
        }
        if x != 0 {
            events.push(sys::InputEvent::new(sys::EV_REL, sys::REL_HWHEEL, x));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.device
            .emit(&events)
            .context("émission d'un défilement")?;
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let pressed = std::mem::take(&mut self.pressed);
        for button in pressed {
            self.device
                .emit(&[sys::InputEvent::new(
                    sys::EV_KEY,
                    button_code(button),
                    0,
                )])
                .context("relâchement des boutons")?;
        }
        Ok(())
    }
}
