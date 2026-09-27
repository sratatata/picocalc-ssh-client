use crate::Irqs;
use embassy_rp::peripherals::TRNG;
use embassy_rp::trng::Trng;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_core::RngCore;

static RNG: OnceLock<Mutex<CriticalSectionRawMutex, Trng<TRNG>>> = OnceLock::new();

pub fn init_rng(trng: TRNG) {
    if RNG
        .init(Mutex::new(Trng::new(
            trng,
            Irqs,
            embassy_rp::trng::Config::default(),
        )))
        .is_err()
    {
        panic!("failed to init Trng");
    }

    getrandom::register_custom_getrandom!(getrandom_custom);
}

fn getrandom_custom(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    let mut rng = PicoRng;
    let mut rng = ChaCha20Rng::from_rng(&mut rng).map_err(|_err| getrandom::Error::UNEXPECTED)?;
    rng.fill_bytes(buf);
    Ok(())
}

/// Locks the global Trng and runs `f` against it.
fn with_rng<T>(f: impl FnOnce(&mut Trng<TRNG>) -> T) -> T {
    let mut rng = RNG.try_get().unwrap().try_lock().unwrap();
    f(&mut rng)
}

/// Our Rng type. It internally manages mutual exclusion around
/// the underlying TRNG hardware.
pub struct PicoRng;
impl rand_core::RngCore for PicoRng {
    fn next_u32(&mut self) -> u32 {
        with_rng(|rng| rng.next_u32())
    }
    fn next_u64(&mut self) -> u64 {
        with_rng(|rng| rng.next_u64())
    }
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        with_rng(|rng| rand_core::RngCore::fill_bytes(rng, buf))
    }
    fn try_fill_bytes(&mut self, buf: &mut [u8]) -> Result<(), rand_core::Error> {
        with_rng(|rng| rng.try_fill_bytes(buf))
    }
}
