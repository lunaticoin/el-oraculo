use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Meta {
    pub last_height: usize,
    pub ref_bin: f64,
}

pub struct PriceStore {
    prices_file: Mutex<File>,
    timestamps_file: Mutex<File>,
    meta_path: PathBuf,
    len: AtomicUsize,
}

impl PriceStore {
    pub fn open(dir: &Path) -> Self {
        std::fs::create_dir_all(dir).expect("Failed to create data directory");

        let prices_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("prices.bin"))
            .expect("Failed to open prices.bin");

        let timestamps_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("timestamps.bin"))
            .expect("Failed to open timestamps.bin");

        let prices_len = prices_file.metadata().unwrap().len() as usize / 8;
        let timestamps_len = timestamps_file.metadata().unwrap().len() as usize / 4;
        let len = prices_len.min(timestamps_len);

        Self {
            prices_file: Mutex::new(prices_file),
            timestamps_file: Mutex::new(timestamps_file),
            meta_path: dir.join("meta.json"),
            len: AtomicUsize::new(len),
        }
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn last_height(&self) -> Option<usize> {
        let len = self.len();
        if len > 0 { Some(len - 1) } else { None }
    }

    pub fn append(&self, price: f64, timestamp: u32) {
        let len = self.len.load(Ordering::Relaxed);

        {
            let mut pf = self.prices_file.lock().unwrap();
            pf.seek(SeekFrom::Start((len * 8) as u64)).unwrap();
            pf.write_all(&price.to_le_bytes()).unwrap();
        }

        {
            let mut tf = self.timestamps_file.lock().unwrap();
            tf.seek(SeekFrom::Start((len * 4) as u64)).unwrap();
            tf.write_all(&timestamp.to_le_bytes()).unwrap();
        }

        self.len.fetch_add(1, Ordering::Release);
    }

    pub fn flush(&self) {
        self.prices_file.lock().unwrap().flush().unwrap();
        self.timestamps_file.lock().unwrap().flush().unwrap();
    }

    pub fn get_price(&self, height: usize) -> Option<f64> {
        if height >= self.len() {
            return None;
        }
        let mut buf = [0u8; 8];
        let mut pf = self.prices_file.lock().unwrap();
        pf.seek(SeekFrom::Start((height * 8) as u64)).ok()?;
        pf.read_exact(&mut buf).ok()?;
        Some(f64::from_le_bytes(buf))
    }

    pub fn get_timestamp(&self, height: usize) -> Option<u32> {
        if height >= self.len() {
            return None;
        }
        let mut buf = [0u8; 4];
        let mut tf = self.timestamps_file.lock().unwrap();
        tf.seek(SeekFrom::Start((height * 4) as u64)).ok()?;
        tf.read_exact(&mut buf).ok()?;
        Some(u32::from_le_bytes(buf))
    }

    pub fn get_prices_range(&self, from: usize, to: usize) -> Vec<(usize, f64, u32)> {
        let len = self.len();
        let to = to.min(len);
        let mut result = Vec::with_capacity(to.saturating_sub(from));
        for h in from..to {
            if let (Some(price), Some(ts)) = (self.get_price(h), self.get_timestamp(h)) {
                result.push((h, price, ts));
            }
        }
        result
    }

    pub fn height_for_timestamp(&self, target_ts: u32) -> Option<usize> {
        let len = self.len();
        if len == 0 {
            return None;
        }

        let mut lo = 0usize;
        let mut hi = len;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.get_timestamp(mid) {
                Some(ts) if ts < target_ts => lo = mid + 1,
                Some(_) => hi = mid,
                None => return None,
            }
        }

        if lo < len { Some(lo) } else { None }
    }

    pub fn save_meta(&self, last_height: usize, ref_bin: f64) {
        let meta = Meta {
            last_height,
            ref_bin,
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        std::fs::write(&self.meta_path, json).unwrap();
    }

    pub fn load_meta(&self) -> Option<Meta> {
        let data = std::fs::read_to_string(&self.meta_path).ok()?;
        serde_json::from_str(&data).ok()
    }
}
