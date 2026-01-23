use crate::backend::{udev::Udev, winit::Winit};

pub mod udev;
pub mod winit;

pub enum Backend {
    Winit(Winit),
    Udev(Udev),
}

impl Backend {
    pub fn udev(&mut self) -> &mut Udev {
        if let Self::Udev(udev) = self {
            udev
        } else {
            unreachable!()
        }
    }

    pub fn winit(&mut self) -> &mut Winit {
        if let Self::Winit(winit) = self {
            winit
        } else {
            unreachable!()
        }
    }
}
