// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

// Copyright © Aptos Foundation

// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    indexer::{
        fetcher::TransactionFetcherOptions, processing_result::{EndpointTransaction, EndpointRequest}, tailer::Tailer
    }
};
use aptos_api::context::Context;
use aptos_config::config::{IndexerConfig, NodeConfig};
use aptos_db_indexer::table_info_reader::TableInfoReader;
use aptos_logger::{error, info};
use aptos_mempool::MempoolClientSender;
use aptos_storage_interface::DbReader;
use aptos_types::chain_id::ChainId;
use serde::{Serialize, Deserialize};
use std::{collections::VecDeque, sync::Arc};
use tokio::runtime::{Builder, Runtime};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message, accept_async};
use futures::{StreamExt, TryStreamExt, TryFutureExt, SinkExt, Stream, FutureExt};

const WEBSOCKET_ENDPOINT: &str = "ws://127.0.0.1:34788/sync";

#[derive(Deserialize)]
struct HandshakeResponse {
    pub version: u64,
    pub events: Vec<String>,
    pub resources: Vec<String>,
    pub handles: Vec<String>
}

/// Creates a runtime which creates a thread pool which reads from storage and writes to postgres
/// Returns corresponding Tokio runtime
pub fn bootstrap(
    config: &NodeConfig,
    chain_id: ChainId,
    db: Arc<dyn DbReader>,
    mp_sender: MempoolClientSender
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
        let context = Arc::new(Context::new(chain_id, db, mp_sender, node_config, None));
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
        TransactionFetcherOptions::new(Some(100), None, Some(batch_size), None, fetch_tasks as usize);

    let url = url::Url::parse(&WEBSOCKET_ENDPOINT).unwrap();
    let (ws, _) = connect_async(url).await.unwrap();

    let (mut write, mut read) = ws.split();

    let handshake: HandshakeResponse = match read.next().await.unwrap() {
        Ok(Message::Text(message)) => serde_json::from_str(&message).unwrap(),
        _ => panic!("Websocket message is not textual")
    };

    let tailer = Tailer::new(context, handshake.events, handshake.resources, handshake.handles, options)
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

            // can't make this shit async, need to await or it doesn't work
            let request = EndpointRequest { transactions };
            if let Err(error) = write.send(Message::Text(serde_json::to_string(&request).unwrap())).await {
                panic!("Could not send transactions: {:?}", error);
            }
        }
    }
}
