use smart_leds::RGB8;

pub mod stars;

pub trait Pattern {
    fn update_rate(&self) -> u64;
    fn update(&mut self, colors: &mut [RGB8]);
}
