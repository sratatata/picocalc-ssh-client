use crate::fixed_str::FixedString;
use crate::screen::SCREEN;
use embassy_rp::flash::{
    Async, ERASE_SIZE, Error as FlashError, Flash as RpFlash, PAGE_SIZE, WRITE_SIZE,
};
use embassy_rp::peripherals::{DMA_CH3, FLASH};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use heapless::FnvIndexMap;
use sequential_storage::cache::NoCache;
use sequential_storage::erase_all;
use sequential_storage::map::{fetch_all_items, fetch_item, remove_item, store_item};

const PICO2_FLASH_SIZE: usize = 4 * 1024 * 1024;
pub const CONFIG_SIZE: u32 = ERASE_SIZE as u32 * 2;
pub const CONFIG_BASE: u32 = PICO2_FLASH_SIZE as u32 - CONFIG_SIZE;
const SCRATCH_SIZE: usize = PAGE_SIZE * 2;
const CONFIG_RANGE: core::ops::Range<u32> = CONFIG_BASE..CONFIG_BASE + CONFIG_SIZE;

pub static CONFIG: LazyLock<Mutex<CriticalSectionRawMutex, Configuration>> =
    LazyLock::new(|| Mutex::new(Configuration::default()));

#[derive(Debug, Default)]
pub struct Configuration {
    flash: Option<Flash>,
}

pub type StrKey = FixedString<32>;
pub type StrValue = FixedString<128>;

impl Configuration {
    pub fn assign_flash(&mut self, flash: Flash) {
        self.flash.replace(flash);
    }

    fn flash_mut(&mut self) -> &mut RpFlash<'static, FLASH, Async, PICO2_FLASH_SIZE> {
        match &mut self.flash {
            Some(flash) => &mut flash.flash,
            None => todo!(),
        }
    }

    fn key_and_buf(
        key: &str,
    ) -> Result<(StrKey, [u8; SCRATCH_SIZE]), sequential_storage::Error<embassy_rp::flash::Error>>
    {
        let key: StrKey = key.try_into()?;
        Ok((key, [0u8; SCRATCH_SIZE]))
    }

    pub async fn fetch(
        &mut self,
        key: &str,
    ) -> Result<Option<StrValue>, sequential_storage::Error<embassy_rp::flash::Error>> {
        let (key, mut buf) = Self::key_and_buf(key)?;
        fetch_item(
            self.flash_mut(),
            CONFIG_RANGE,
            &mut NoCache::new(),
            &mut buf,
            &key,
        )
        .await
    }

    pub async fn remove(
        &mut self,
        key: &str,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        let (key, mut buf) = Self::key_and_buf(key)?;
        remove_item(
            self.flash_mut(),
            CONFIG_RANGE,
            &mut NoCache::new(),
            &mut buf,
            &key,
        )
        .await
    }

    pub async fn store(
        &mut self,
        key: &str,
        value: StrValue,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        let (key, mut buf) = Self::key_and_buf(key)?;
        store_item(
            self.flash_mut(),
            CONFIG_RANGE,
            &mut NoCache::new(),
            &mut buf,
            &key,
            &value,
        )
        .await
    }

    pub async fn format(
        &mut self,
    ) -> Result<(), sequential_storage::Error<embassy_rp::flash::Error>> {
        erase_all(self.flash_mut(), CONFIG_RANGE).await
    }

    pub async fn get_all(
        &mut self,
    ) -> Result<
        FnvIndexMap<StrKey, StrValue, 32>,
        sequential_storage::Error<embassy_rp::flash::Error>,
    > {
        let mut buf = [0u8; SCRATCH_SIZE];
        let mut cache = NoCache::new();
        let mut iter =
            fetch_all_items::<StrKey, _, _>(self.flash_mut(), CONFIG_RANGE, &mut cache, &mut buf)
                .await?;

        let mut map = FnvIndexMap::new();

        while let Some((key, value)) = iter.next::<StrKey, StrValue>(&mut buf).await? {
            if let Err((k, v)) = map.insert(key, value) {
                print!("Configuration::get_all: too many keys. Ignoring {k} -> {v}\r\n");
            }
        }

        Ok(map)
    }
}

pub struct Flash {
    flash: RpFlash<'static, FLASH, Async, PICO2_FLASH_SIZE>,
}

impl core::fmt::Debug for Flash {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        fmt.debug_struct("Flash").finish()
    }
}

impl Flash {
    pub fn new(flash: FLASH, dma: DMA_CH3) -> Self {
        let flash = embassy_rp::flash::Flash::new(flash, dma);
        log::info!(
            "flash capacity={}, write={}, erase={}",
            flash.capacity(),
            WRITE_SIZE,
            ERASE_SIZE
        );
        Self { flash }
    }

    #[allow(unused)]
    pub async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), FlashError> {
        self.flash.read(offset, bytes).await
    }
}

pub async fn config_command(args: &[&str]) {
    match args {
        ["config", "format"] => {
            let mut config = CONFIG.get().lock().await;
            let result = config.format().await;
            print!("{result:?}");
        }
        ["config", "list"] => {
            let map = {
                let mut config = CONFIG.get().lock().await;
                config.get_all().await
            };
            match map {
                Ok(map) => {
                    for (k, v) in &map {
                        match crate::mic::effective_setting(k.as_str()).await {
                            Some(effective) => print!("{k}={effective}\r\n"),
                            None => print!("{k}={v}\r\n"),
                        }
                    }
                }
                Err(err) => {
                    print!("{err:?}\r\n");
                }
            }
        }
        ["config", "get", key] => {
            if *key == "scroll" {
                let mut config = CONFIG.get().lock().await;
                match config.fetch(key).await {
                    Ok(Some(val)) => print!("{val}\r\n"),
                    Ok(None) => {
                        let default = SCREEN.get().lock().await.max_scrollback();
                        print!("{default}\r\n");
                    }
                    Err(e) => print!("{e:?}\r\n"),
                }
                return;
            }
            // Mic debug keys are optional and have effective defaults (see
            // `mic::load_settings`), so report the value the next recording
            // would actually use rather than the raw store slot.
            if let Some(value) = crate::mic::effective_setting(key).await {
                print!("{value}\r\n");
                return;
            }
            // The ssh audio helper command has a default too, and an empty
            // stored value disables the transport; report that resolved state.
            if *key == crate::net::PTT_SSH_CMD_KEY {
                match crate::net::effective_ssh_audio_command().await {
                    Some(command) => print!("{}\r\n", command.describe()),
                    None => print!("(disabled)\r\n"),
                }
                return;
            }
            let mut config = CONFIG.get().lock().await;
            let value = config.fetch(key).await;
            print!("{value:?}\r\n");
        }
        ["config", "rm", key] => {
            if *key == "scroll" {
                let mut screen = SCREEN.get().lock().await;
                let default = screen.max_safe_scrollback();
                screen.set_max_scrollback(default);
            }
            let mut config = CONFIG.get().lock().await;
            let result = config.remove(key).await;
            print!("{result:?}\r\n");
        }
        ["config", "set", key, rest @ ..] => {
            let value: &str = &rest.join(" ");
            // Refuse a mic debug value that would mis-clock the device before
            // it ever reaches flash (see `mic::validate_config_setting`).
            if let Err(message) = crate::mic::validate_config_setting(key, value).await {
                print!("{message}\r\n");
                return;
            }
            if *key == "scroll" {
                if let Ok(val) = value.parse::<usize>() {
                    // Heap-aware ceiling, not a flat number: keeps the screen's
                    // logical footprint within its heap budget regardless of
                    // screen geometry (see terminal-model's
                    // SCREEN_HEAP_BUDGET_BYTES / max_safe_scrollback).
                    let max_safe = SCREEN.get().lock().await.max_safe_scrollback();
                    if val <= max_safe {
                        SCREEN.get().lock().await.set_max_scrollback(val);
                    } else {
                        print!("scroll value must be <= {max_safe} (heap-budget limit)\r\n");
                        return;
                    }
                } else {
                    print!("scroll value must be a number\r\n");
                    return;
                }
            }
            let value: StrValue = match (*value).try_into() {
                Ok(v) => v,
                Err(err) => {
                    print!("value `{value}`: {err:?}\r\n");
                    return;
                }
            };
            let mut config = CONFIG.get().lock().await;
            match config.store(key, value).await {
                Ok(()) => {
                    print!("OK\r\n");
                }
                Err(err) => {
                    print!("{err:?}\r\n");
                }
            }
        }
        _ => {
            print!("invalid arguments\r\n");
        }
    }
}
