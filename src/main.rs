#![no_std]
#![no_main]

use bsp::{entry, hal::timer::Alarm};
use defmt::*;
use defmt_rtt as _;
use embedded_hal::digital::v2::ToggleableOutputPin;
use panic_probe as _;

use rp_pico as bsp;

use bsp::hal::{clocks::init_clocks_and_plls, gpio, pac, sio::Sio, timer, watchdog};

use gpio::{pin::bank0::Gpio25, pin::Output, pin::PushPull};

use pac::interrupt;

use core::cell::RefCell;
use core::ops::DerefMut;
use cortex_m::interrupt::{free, Mutex};

use fugit::MicrosDurationU32;

static ALARM0: Mutex<RefCell<Option<timer::Alarm0>>> = Mutex::new(RefCell::new(None));
static LED: Mutex<RefCell<Option<gpio::Pin<Gpio25, Output<PushPull>>>>> =
    Mutex::new(RefCell::new(None));

#[entry]
fn main() -> ! {
    info!("Program start");
    let mut pac = pac::Peripherals::take().unwrap();
    let sio = Sio::new(pac.SIO);
    let mut watchdog = watchdog::Watchdog::new(pac.WATCHDOG);
    init_clocks_and_plls(
        bsp::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let pins = bsp::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let led_pin = pins.led.into_push_pull_output();

    let mut timer = timer::Timer::new(pac.TIMER, &mut pac.RESETS);
    let mut alarm0 = timer.alarm_0().unwrap();

    free(|cs| {
        alarm0.enable_interrupt();
        alarm0.clear_interrupt();
        alarm0
            .schedule(MicrosDurationU32::from_ticks(1_000_000))
            .unwrap();
        ALARM0.borrow(cs).replace(Some(alarm0));
        LED.borrow(cs).replace(Some(led_pin));
    });

    unsafe {
        pac::NVIC::unmask(pac::Interrupt::TIMER_IRQ_0);
    }

    loop {}
}

#[interrupt]
fn TIMER_IRQ_0() {
    free(|cs| {
        if let (Some(alarm0), Some(led)) = (
            ALARM0.borrow(cs).borrow_mut().deref_mut(),
            LED.borrow(cs).borrow_mut().deref_mut(),
        ) {
            alarm0.clear_interrupt();
            alarm0
                .schedule(MicrosDurationU32::from_ticks(1_000_000))
                .unwrap();

            led.toggle().unwrap();
        }
    })
}
