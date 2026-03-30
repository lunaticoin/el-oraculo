use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brk_oracle::{cents_to_bin, Config, Oracle, PRICES, START_HEIGHT};
use brk_reader::Reader;
use brk_rpc::{backend::Auth, Client};
use brk_types::{Block, Height};
use tracing::{error, info};

use crate::storage::PriceStore;

pub struct SyncConfig {
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_pass: String,
    pub blocks_dir: Option<PathBuf>,
}

pub async fn run_sync(
    store: Arc<PriceStore>,
    config: SyncConfig,
    chain_tip: Arc<AtomicUsize>,
) {
    let sync = move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = Client::new(
            &config.rpc_url,
            Auth::UserPass(config.rpc_user.clone(), config.rpc_pass.clone()),
        )?;

        info!("Waiting for Bitcoin Core to sync...");
        client.wait_for_synced_node()?;

        let tip = client.get_last_height()?;
        let tip_usize: usize = (*tip).try_into().unwrap();
        chain_tip.store(tip_usize, Ordering::Relaxed);
        info!("Bitcoin Core synced at height {}", tip_usize);

        let stored = store.last_height();
        let start = stored.map(|h| h + 1).unwrap_or(0);

        // Detect if we can use brk_reader
        let use_reader = config.blocks_dir.as_ref().is_some_and(|d| d.exists());
        if use_reader {
            info!("Fast mode: reading blocks from disk via brk_reader");
        } else {
            info!("Slow mode: reading blocks via RPC (mount blocks dir for faster sync)");
        }

        // Phase 1: Bootstrap from embedded PRICES (blocks 0..START_HEIGHT)
        if start < START_HEIGHT {
            info!(
                "Phase 1: Loading embedded prices for blocks {}..{}",
                start,
                START_HEIGHT - 1
            );
            let lines: Vec<&str> = PRICES.lines().collect();

            // Phase 1 always uses RPC headers (fast, only 80 bytes per block)
            for height in start..START_HEIGHT.min(lines.len()) {
                let price: f64 = lines[height].parse().unwrap_or(0.0);
                let hash = client.get_block_hash(height as u64)?;
                let header = client.get_block_header(&hash)?;
                store.append(price, header.time);

                if height % 10_000 == 0 {
                    info!("Phase 1: block {} / {}", height, START_HEIGHT);
                    store.flush();
                }
            }

            let prices_len = lines.len();
            if prices_len < START_HEIGHT {
                for height in start.max(prices_len)..START_HEIGHT {
                    let price: f64 = lines.last().map(|l| l.parse().unwrap_or(0.0)).unwrap_or(0.0);
                    let hash = client.get_block_hash(height as u64)?;
                    let header = client.get_block_header(&hash)?;
                    store.append(price, header.time);

                    if height % 10_000 == 0 {
                        info!("Phase 1: block {} / {}", height, START_HEIGHT);
                        store.flush();
                    }
                }
            }

            store.flush();
            info!("Phase 1 complete: {} blocks bootstrapped", START_HEIGHT);
        }

        // Phase 2: Initialize oracle
        let resume_height = start.max(START_HEIGHT);

        let oracle = if let Some(meta) = store.load_meta() {
            info!(
                "Restoring oracle from checkpoint at height {}, ref_bin={}",
                meta.last_height, meta.ref_bin
            );
            let warmup_start = resume_height.saturating_sub(12);
            Oracle::from_checkpoint(meta.ref_bin, Config::default(), |oracle| {
                for h in warmup_start..resume_height {
                    if let Ok(block) = fetch_block(&client, h) {
                        oracle.process_block(&block);
                    }
                }
            })
        } else {
            let seed_line = PRICES
                .lines()
                .nth(START_HEIGHT.saturating_sub(1))
                .unwrap_or("50000.0");
            let seed_price: f64 = seed_line.parse().unwrap_or(50000.0);
            let start_bin = cents_to_bin(seed_price * 100.0);
            info!(
                "Initializing oracle at seed price ${:.2} (bin={:.2})",
                seed_price, start_bin
            );

            if resume_height > START_HEIGHT {
                let mut oracle = Oracle::new(start_bin, Config::default());
                info!(
                    "Phase 2: Catching up oracle from {} to {}",
                    START_HEIGHT, resume_height
                );

                if use_reader {
                    let reader = Reader::new(
                        config.blocks_dir.clone().unwrap(),
                        &client,
                    );
                    let rx = reader.read(
                        Some(Height::from(START_HEIGHT as u32)),
                        Some(Height::from((resume_height - 1) as u32)),
                    );
                    for read_block in rx {
                        let block: Block = read_block.into();
                        let height = *block.height() as usize;
                        oracle.process_block(&block);
                        if height % 50_000 == 0 {
                            info!("Phase 2 catch-up: block {}", height);
                        }
                    }
                } else {
                    for h in START_HEIGHT..resume_height {
                        if let Ok(block) = fetch_block(&client, h) {
                            oracle.process_block(&block);
                        }
                        if h % 10_000 == 0 {
                            info!("Phase 2 catch-up: block {}", h);
                        }
                    }
                }
                oracle
            } else {
                Oracle::new(start_bin, Config::default())
            }
        };

        // Phase 3: Process new blocks
        let current_tip = client.get_last_height()?;
        let current_tip_usize: usize = (*current_tip).try_into().unwrap();
        chain_tip.store(current_tip_usize, Ordering::Relaxed);

        if resume_height <= current_tip_usize {
            info!(
                "Phase 3: Processing blocks {}..{}",
                resume_height, current_tip_usize
            );
            let mut oracle = oracle;

            if use_reader {
                // Fast path: read from disk
                let reader = Reader::new(
                    config.blocks_dir.clone().unwrap(),
                    &client,
                );
                let rx = reader.read(
                    Some(Height::from(resume_height as u32)),
                    Some(Height::from(current_tip_usize as u32)),
                );
                for read_block in rx {
                    let block: Block = read_block.into();
                    let height = *block.height() as usize;
                    let timestamp = block.header.time;
                    oracle.process_block(&block);
                    let price = *oracle.price_dollars();

                    store.append(price, timestamp);

                    if height % 10_000 == 0 {
                        store.flush();
                        store.save_meta(height, oracle.ref_bin());
                        info!(
                            "Phase 3: block {} / {} (${:.2})",
                            height, current_tip_usize, price
                        );
                    }
                }
            } else {
                // Slow path: sequential RPC
                for height in resume_height..=current_tip_usize {
                    let block = fetch_block(&client, height)?;
                    let timestamp = block.header.time;
                    oracle.process_block(&block);
                    let price = *oracle.price_dollars();

                    store.append(price, timestamp);

                    if height % 1_000 == 0 {
                        store.flush();
                        store.save_meta(height, oracle.ref_bin());
                        info!(
                            "Phase 3: block {} / {} (${:.2})",
                            height, current_tip_usize, price
                        );
                    }
                }
            }

            store.flush();
            store.save_meta(current_tip_usize, oracle.ref_bin());
            info!(
                "Sync complete at height {}. Price: ${:.2}",
                current_tip_usize,
                *oracle.price_dollars()
            );

            poll_loop(client, oracle, store, chain_tip);
        } else {
            info!("Already synced to tip {}", resume_height - 1);
            poll_loop(client, oracle, store, chain_tip);
        }

        Ok(())
    };

    if let Err(e) = sync() {
        error!("Sync failed: {}", e);
    }
}

fn poll_loop(
    client: Client,
    mut oracle: Oracle,
    store: Arc<PriceStore>,
    chain_tip: Arc<AtomicUsize>,
) {
    info!("Entering poll mode...");
    loop {
        std::thread::sleep(Duration::from_secs(30));

        match client.get_last_height() {
            Ok(tip) => {
                let tip_usize: usize = (*tip).try_into().unwrap();
                chain_tip.store(tip_usize, Ordering::Relaxed);

                let stored = store.last_height().unwrap_or(0);
                if tip_usize > stored {
                    for height in (stored + 1)..=tip_usize {
                        match fetch_block(&client, height) {
                            Ok(block) => {
                                let timestamp = block.header.time;
                                oracle.process_block(&block);
                                let price = *oracle.price_dollars();
                                store.append(price, timestamp);
                                info!("New block {}: ${:.2}", height, price);
                            }
                            Err(e) => {
                                error!("Failed to fetch block {}: {}", height, e);
                                break;
                            }
                        }
                    }
                    store.flush();
                    store.save_meta(tip_usize, oracle.ref_bin());
                }
            }
            Err(e) => {
                error!("RPC error during poll: {}", e);
            }
        }
    }
}

fn fetch_block(client: &Client, height: usize) -> Result<Block, Box<dyn std::error::Error + Send + Sync>> {
    let hash = client.get_block_hash(height as u64)?;
    let btc_block = client.get_block(&hash)?;
    Ok(Block::from((Height::from(height as u32), btc_block)))
}
