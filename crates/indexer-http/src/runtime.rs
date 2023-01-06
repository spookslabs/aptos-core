// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    indexer::{
        fetcher::TransactionFetcherOptions, processing_result::{EndpointTransaction, EndpointRequest}, tailer::Tailer
    }
};
use aptos_api::context::Context;
use aptos_config::config::{IndexerConfig, NodeConfig};
use aptos_logger::{error, info};
use aptos_mempool::MempoolClientSender;
use aptos_storage_interface::DbReader;
use aptos_types::chain_id::ChainId;
use serde::{Serialize, Deserialize};
use std::{collections::VecDeque, sync::Arc};
use tokio::runtime::{Builder, Runtime};

const ENDPOINT: &str = "http://127.0.0.1:34789/aptos";

#[derive(Deserialize)]
struct HandshakeResponse {
    pub version: u64,
    pub events: Vec<String>,
    pub handles: Vec<String>
}

pub struct MovingAverage {
    window_millis: u64,
    // (timestamp_millis, value)
    values: VecDeque<(u64, u64)>,
    sum: u64,
}

impl MovingAverage {
    pub fn new(window_millis: u64) -> Self {
        Self {
            window_millis,
            values: VecDeque::new(),
            sum: 0,
        }
    }

    pub fn tick_now(&mut self, value: u64) {
        let now = chrono::Utc::now().naive_utc().timestamp_millis() as u64;
        self.tick(now, value);
    }

    pub fn tick(&mut self, timestamp_millis: u64, value: u64) -> f64 {
        self.values.push_back((timestamp_millis, value));
        self.sum += value;
        loop {
            match self.values.front() {
                None => break,
                Some((ts, val)) => {
                    if timestamp_millis - ts > self.window_millis {
                        self.sum -= val;
                        self.values.pop_front();
                    } else {
                        break;
                    }
                },
            }
        }
        self.avg()
    }

    pub fn avg(&self) -> f64 {
        if self.values.len() < 2 {
            0.0
        } else {
            let elapsed = self.values.back().unwrap().0 - self.values.front().unwrap().0;
            self.sum as f64 / elapsed as f64
        }
    }
}

/// Creates a runtime which creates a thread pool which reads from storage and writes to postgres
/// Returns corresponding Tokio runtime
pub fn bootstrap(
    config: &NodeConfig,
    chain_id: ChainId,
    db: Arc<dyn DbReader>,
    mp_sender: MempoolClientSender,
) -> Option<anyhow::Result<Runtime>> {
    if !config.indexer.enabled {
        return None;
    }

    let runtime = Builder::new_multi_thread()
        .thread_name("indexer")
        .disable_lifo_slot()
        .enable_all()
        .build()
        .expect("[indexer] failed to create runtime");

    let indexer_config = config.indexer.clone();
    let node_config = config.clone();

    runtime.spawn(async move {
        let context = Arc::new(Context::new(chain_id, db, mp_sender, node_config));
        run_forever(indexer_config, context).await;
    });

    Some(Ok(runtime))
}

pub async fn run_forever(config: IndexerConfig, context: Arc<Context>) {
    // All of these options should be filled already with defaults
    let processor_name = config.processor.clone().unwrap();
    let fetch_tasks = config.fetch_tasks.unwrap();
    let processor_tasks = config.processor_tasks.unwrap();
    let emit_every = config.emit_every.unwrap();
    let batch_size = config.batch_size.unwrap();

    info!(processor_name = processor_name, "Starting indexer...");

    info!(processor_name = processor_name, "Instantiating tailer... ");

    let options =
        TransactionFetcherOptions::new(None, None, Some(batch_size), None, fetch_tasks as usize);

    let client = reqwest::Client::new();

    let response = match client.get(ENDPOINT).send().await {
        Ok(res) => res,
        Err(err) => panic!("Could not reach handshake endpoint: {:?}", err),
    };

    let handshake: HandshakeResponse = match response.json().await {
        Ok(res) => res,
        Err(err) => panic!("Could not parse handshake response: {:?}", err),
    };

    let tailer = Tailer::new(context, handshake.events, handshake.handles, options)
        .expect("Failed to instantiate tailer");

    let start_version = handshake.version;

    info!(
        processor_name = processor_name,
        final_start_version = start_version,
        start_version_from_config = config.starting_version,
        "Setting starting version..."
    );
    tailer.set_fetcher_version(start_version as u64).await;

    info!(processor_name = processor_name, "Starting fetcher...");
    tailer.transaction_fetcher.lock().await.start().await;

    info!(
        processor_name = processor_name,
        start_version = start_version,
        "Indexing loop started!"
    );

    let mut versions_processed: u64 = 0;
    let mut base: u64 = 0;

    let mut ma = MovingAverage::new(10_000);

    loop {
        let mut tasks = vec![];
        for _ in 0..processor_tasks {
            let other_tailer = tailer.clone();
            let task = tokio::spawn(async move { other_tailer.process_next_batch().await });
            tasks.push(task);
        }
        let batches = match futures::future::try_join_all(tasks).await {
            Ok(res) => res,
            Err(err) => panic!("Error processing transaction batches: {:?}", err),
        };

        let mut transactions: Vec<EndpointTransaction> = vec![];

        let mut batch_start_version = u64::MAX;
        let mut batch_end_version = 0;
        let mut num_res = 0;

        for (num_txn, res) in batches {
            if let Some(processed_result) = res {
                batch_start_version =
                    std::cmp::min(batch_start_version, processed_result.start_version);
                batch_end_version = std::cmp::max(batch_end_version, processed_result.end_version);
                num_res += num_txn;
                transactions.extend(processed_result.transactions);
            };
        }

        if !transactions.is_empty() {

            info!(
                versions = transactions.iter().map(|transaction| transaction.version).collect::<Vec<u64>>(),
                "Sending transactions to endpoint"
            );

            transactions.sort_by(|a, b| a.version.cmp(&b.version));
            let request = EndpointRequest { transactions };
            if let Err(err) = client.post(ENDPOINT).json(&request).send().await {
                panic!("Could not reach endpoint: {:?}", err);
            }
        }

        ma.tick_now(num_res);

        versions_processed += num_res;
        if emit_every != 0 {
            let new_base: u64 = versions_processed / (emit_every as u64);
            if base != new_base {
                base = new_base;
                info!(
                    processor_name = processor_name,
                    batch_start_version = batch_start_version,
                    batch_end_version = batch_end_version,
                    versions_processed = versions_processed,
                    tps = (ma.avg() * 1000.0) as u64,
                    "Processed batch version"
                );
            }
        }
    }
}
