//! This example showcases channels and timing utilities of embassy.
//!
//! This example works best on STM32F3DISCOVERY board. It flashes a single LED, then if the USER
//! button is pressed the next LED is flashed (in clockwise fashion). If the USER button is double
//! clicked, then the direction changes. If the USER button is pressed for 1 second, then all the
//! LEDS flash 3 times.
//!

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use defmt::*;
use embassy::blocking_mutex::raw::ThreadModeRawMutex;
use embassy::channel::mpmc::Channel;
use embassy::executor::Spawner;
use embassy::time::{with_timeout, Duration, Timer};
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::gpio::{AnyPin, Input, Level, Output, Pin, Pull, Speed};
use embassy_stm32::peripherals::PA0;
use embassy_stm32::Peripherals;
use {defmt_rtt as _, panic_probe as _};

struct Leds<'a> {
    leds: [Output<'a, AnyPin>; 8],
    direction: i8,
    current_led: usize,
}

impl<'a> Leds<'a> {
    fn new(pins: [Output<'a, AnyPin>; 8]) -> Self {
        Self {
            leds: pins,
            direction: 1,
            current_led: 0,
        }
    }

    fn change_direction(&mut self) {
        self.direction *= -1;
    }

    fn move_next(&mut self) {
        if self.direction > 0 {
            self.current_led = (self.current_led + 1) & 7;
        } else {
            self.current_led = (8 + self.current_led - 1) & 7;
        }
    }

    async fn show(&mut self) {
        self.leds[self.current_led].set_high();
        if let Ok(new_message) = with_timeout(Duration::from_millis(500), CHANNEL.recv()).await {
            self.leds[self.current_led].set_low();
            self.process_event(new_message).await;
        } else {
            self.leds[self.current_led].set_low();
            if let Ok(new_message) = with_timeout(Duration::from_millis(200), CHANNEL.recv()).await {
                self.process_event(new_message).await;
            }
        }
    }

    async fn flash(&mut self) {
        for _ in 0..3 {
            for led in &mut self.leds {
                led.set_high();
            }
            Timer::after(Duration::from_millis(500)).await;
            for led in &mut self.leds {
                led.set_low();
            }
            Timer::after(Duration::from_millis(200)).await;
        }
    }

    async fn process_event(&mut self, event: ButtonEvent) {
        match event {
            ButtonEvent::SingleClick => {
                self.move_next();
            }
            ButtonEvent::DoubleClick => {
                self.change_direction();
                self.move_next();
            }
            ButtonEvent::Hold => {
                self.flash().await;
            }
        }
    }
}

#[derive(Format)]
enum ButtonEvent {
    SingleClick,
    DoubleClick,
    Hold,
}

static CHANNEL: Channel<ThreadModeRawMutex, ButtonEvent, 4> = Channel::new();

#[embassy::main]
async fn main(spawner: Spawner, p: Peripherals) {
    let button = Input::new(p.PA0, Pull::Down);
    let button = ExtiInput::new(button, p.EXTI0);
    info!("Press the USER button...");
    let leds = [
        Output::new(p.PE9.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE10.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE11.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE12.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE13.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE14.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE15.degrade(), Level::Low, Speed::Low),
        Output::new(p.PE8.degrade(), Level::Low, Speed::Low),
    ];
    let leds = Leds::new(leds);

    spawner.spawn(button_waiter(button)).unwrap();
    spawner.spawn(led_blinker(leds)).unwrap();
}

#[embassy::task]
async fn led_blinker(mut leds: Leds<'static>) {
    loop {
        leds.show().await;
    }
}

#[embassy::task]
async fn button_waiter(mut button: ExtiInput<'static, PA0>) {
    const DOUBLE_CLICK_DELAY: u64 = 250;
    const HOLD_DELAY: u64 = 1000;

    button.wait_for_rising_edge().await;
    loop {
        if with_timeout(Duration::from_millis(HOLD_DELAY), button.wait_for_falling_edge())
            .await
            .is_err()
        {
            info!("Hold");
            CHANNEL.send(ButtonEvent::Hold).await;
            button.wait_for_falling_edge().await;
        } else if with_timeout(Duration::from_millis(DOUBLE_CLICK_DELAY), button.wait_for_rising_edge())
            .await
            .is_err()
        {
            info!("Single click");
            CHANNEL.send(ButtonEvent::SingleClick).await;
        } else {
            info!("Double click");
            CHANNEL.send(ButtonEvent::DoubleClick).await;
            button.wait_for_falling_edge().await;
        }
        button.wait_for_rising_edge().await;
    }
}
