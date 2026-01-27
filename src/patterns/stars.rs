use core::ops::ControlFlow;

use esp_hal::rng::Rng;
use heapless::Vec;
use rand::Rng as _;
use smart_leds::RGB8;

use crate::COLOR;

use super::Pattern;

const NUM_STARS: usize = 16;

#[derive(Default)]
pub struct Stars {
    rng: Rng,
    stars: Vec<Star, NUM_STARS>,
}

pub struct Star {
    index: usize,
    value: u8,
    speed: i8,
    max: u8,
    increasing: bool,
    color: RGB8,
}

impl Stars {
    fn add_star(&mut self, len: usize) {
        let index = self.rng.random_range(0..len);

        if self.stars.iter().any(|s| s.index == index) {
            // whatever, we'll try again next tick
            return;
        }

        let speed = self.rng.random_range(2..8);
        let max = self.rng.random_range(200..255);

        self.stars
            .push(Star {
                index,
                value: 0,
                speed,
                max,
                increasing: true,
                color: COLOR.get(),
            })
            .ok();
    }
}

impl Star {
    fn update(&mut self) -> ControlFlow<(), ()> {
        if self.increasing {
            self.value = self.value.saturating_add(self.speed as u8);

            if self.value >= self.max {
                self.value = self.max;
                self.increasing = false;
            }

            ControlFlow::Continue(())
        } else {
            self.value = self.value.saturating_sub(self.speed as u8);

            if self.value == 0 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    }
}

impl Pattern for Stars {
    fn update_rate(&self) -> u64 {
        40
    }

    fn update(&mut self, colors: &mut [RGB8]) {
        if !self.stars.is_full() {
            self.add_star(colors.len());
        }

        self.stars.retain_mut(|star| match star.update() {
            ControlFlow::Continue(()) => {
                colors[star.index] = RGB8 {
                    r: mul_u8(star.color.r, star.value),
                    g: mul_u8(star.color.g, star.value),
                    b: mul_u8(star.color.b, star.value),
                };

                true
            }
            ControlFlow::Break(()) => {
                colors[star.index] = RGB8::default();

                false
            }
        });
    }
}

fn mul_u8(a: u8, b: u8) -> u8 {
    let temp = (a as u16) * (b as u16);
    ((temp + 127) / 255) as u8
}
