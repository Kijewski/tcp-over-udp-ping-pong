use std::sync::OnceLock;
use std::task::Waker;
use std::time::Duration;

use embassy_time_driver::{Driver, time_driver_impl};
use tokio::spawn;
use tokio::time::{Instant, sleep_until};

struct MyDriver {}

impl Driver for MyDriver {
    fn now(&self) -> u64 {
        start().elapsed().as_nanos().try_into().unwrap()
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        let waker = waker.clone();
        let deadline = start().checked_add(Duration::from_nanos(at)).unwrap();
        spawn(async move {
            sleep_until(deadline).await;
            waker.wake_by_ref();
        });
    }
}

fn start() -> Instant {
    static VALUE: OnceLock<Instant> = OnceLock::new();

    *VALUE.get_or_init(Instant::now)
}

time_driver_impl!(static DRIVER: MyDriver = MyDriver{});
