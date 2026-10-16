use std::thread;
use std::time::Duration;

use rand::rngs::{StdRng, SysRng};
use rand::{Rng, SeedableRng};

/// Contract for sleeping a specified duration.
pub trait Sleeper: Send {
    fn sleep(&mut self, duration: Duration);
}

/// Contract for sampling a random delay between minimum and maximum inclusive.
pub trait DelaySource: Send {
    fn sample_inclusive(&mut self, minimum: Duration, maximum: Duration) -> Duration;
}

pub enum SleepPolicy {
    Thread,
    #[cfg(test)]
    Recording(RecordingSleeper),
}

impl Sleeper for SleepPolicy {
    fn sleep(&mut self, duration: Duration) {
        match self {
            Self::Thread => thread::sleep(duration),
            #[cfg(test)]
            Self::Recording(sleeper) => sleeper.sleep(duration),
        }
    }
}

pub struct RandomPolicy {
    random: StdRng,
}

impl RandomPolicy {
    pub fn seeded(seed: u64) -> Self {
        Self {
            random: StdRng::seed_from_u64(seed),
        }
    }

    pub fn from_entropy() -> Self {
        let mut rng = SysRng;
        Self {
            random: StdRng::try_from_rng(&mut rng)
                .expect("failed to initialize StdRng from SysRng"),
        }
    }
}

impl DelaySource for RandomPolicy {
    fn sample_inclusive(&mut self, minimum: Duration, maximum: Duration) -> Duration {
        let minimum = minimum.as_nanos();
        let width = maximum.as_nanos() - minimum + 1;
        let threshold = width.wrapping_neg() % width;
        let offset = loop {
            let candidate =
                u128::from(self.random.next_u64()) << 64 | u128::from(self.random.next_u64());
            if candidate >= threshold {
                break candidate % width;
            }
        };
        duration_from_nanos(minimum + offset)
    }
}

fn duration_from_nanos(nanoseconds: u128) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let seconds = u64::try_from(nanoseconds / NANOS_PER_SECOND)
        .expect("a sampled Duration always has representable seconds");
    let subsecond = u32::try_from(nanoseconds % NANOS_PER_SECOND)
        .expect("a nanosecond remainder is always less than one second");
    Duration::new(seconds, subsecond)
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct RecordingSleeper {
    durations: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
}

#[cfg(test)]
impl RecordingSleeper {
    pub fn observations(&self) -> std::sync::Arc<std::sync::Mutex<Vec<Duration>>> {
        std::sync::Arc::clone(&self.durations)
    }
}

#[cfg(test)]
impl Sleeper for RecordingSleeper {
    fn sleep(&mut self, duration: Duration) {
        if let Ok(mut durations) = self.durations.lock() {
            durations.push(duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DelaySource, RandomPolicy};

    #[test]
    fn seeded_delays_are_reproducible_and_include_valid_bounds() {
        let mut first = RandomPolicy::seeded(42);
        let mut second = RandomPolicy::seeded(42);
        let minimum = Duration::from_nanos(7);
        let maximum = Duration::from_nanos(9);
        let first_samples = (0..64)
            .map(|_| first.sample_inclusive(minimum, maximum))
            .collect::<Vec<_>>();
        let second_samples = (0..64)
            .map(|_| second.sample_inclusive(minimum, maximum))
            .collect::<Vec<_>>();

        assert_eq!(first_samples, second_samples);
        assert!(first_samples.iter().all(|sample| *sample >= minimum));
        assert!(first_samples.iter().all(|sample| *sample <= maximum));
        assert!(first_samples.contains(&minimum));
        assert!(first_samples.contains(&maximum));
    }

    #[test]
    fn full_duration_values_do_not_overflow_sampling_arithmetic() {
        let mut random = RandomPolicy::seeded(9);
        let minimum = Duration::new(u64::MAX, 999_999_998);
        let maximum = Duration::MAX;

        for _ in 0..8 {
            let sample = random.sample_inclusive(minimum, maximum);
            assert!((minimum..=maximum).contains(&sample));
        }
    }
}
