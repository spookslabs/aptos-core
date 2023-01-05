// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0
use crate::{
    indexer::{
        errors::TransactionProcessingError,
        fetcher::{TransactionFetcher, TransactionFetcherOptions, TransactionFetcherTrait},
        processing_result::ProcessingResult
    }
};
use anyhow::{ensure, Context, Result};
use aptos_api::context::Context as ApiContext;
use aptos_logger::{debug, info};
use chrono::ParseError;
use serde::{Serialize, Deserialize};
use std::{fmt::Debug, sync::Arc};
use tokio::{sync::Mutex, task::JoinHandle};
use aptos_api_types::Transaction;

#[derive(Serialize, Deserialize)]
struct EndpointRequest {
    transactions: Vec<EndpointTransaction>
}

#[derive(Serialize, Deserialize)]
struct EndpointTransaction {
    version: u64,
    timestamp: u64,
    events: Vec<EndpointEvent>
}

#[derive(Serialize, Deserialize)]
struct EndpointEvent {
    address: String,
    #[serde(rename = "type")]
    typ: String,
    data: serde_json::Value
}

#[derive(Clone)]
pub struct Tailer {
    pub transaction_fetcher: Arc<Mutex<dyn TransactionFetcherTrait>>,
    client: reqwest::Client,
    endpoint: String,
    events: Vec<String>
}

impl Tailer {
    pub fn new(
        context: Arc<ApiContext>,
        endpoint: String,
        events: Vec<String>,
        options: TransactionFetcherOptions,
    ) -> Result<Tailer, ParseError> {
        let resolver = Arc::new(context.move_resolver().unwrap());
        let transaction_fetcher = TransactionFetcher::new(context, resolver, 0, options);

        Ok(Self {
            transaction_fetcher: Arc::new(Mutex::new(transaction_fetcher)),
            client: reqwest::Client::new(),
            endpoint,
            events
        })
    }

    pub async fn set_fetcher_version(&self, version: u64) {
        self.transaction_fetcher
            .lock()
            .await
            .set_version(version)
            .await;
        info!(version = version, "Will start fetching from version");
    }

    pub async fn process_next_batch(
        &self,
    ) -> (
        u64,
        Option<Result<ProcessingResult, TransactionProcessingError>>,
    ) {
        let transactions = self
            .transaction_fetcher
            .lock()
            .await
            .fetch_next_batch()
            .await;

        let num_txns = transactions.len() as u64;
        // When the batch is empty b/c we're caught up
        if num_txns == 0 {
            return (0, None);
        }
        let start_version = transactions.first().unwrap().version();
        let end_version = transactions.last().unwrap().version();

        debug!(
            num_txns = num_txns,
            start_version = start_version,
            end_version = end_version,
            "Starting processing of transaction batch"
        );

        let batch_start = chrono::Utc::now().naive_utc();

        // instead of using a processor filter by events and only send requested events

        let mut request = EndpointRequest {
            transactions: vec![]
        };

        for transaction in &transactions {
            if let Transaction::UserTransaction(transaction) = transaction {

                let mut events = vec![];
                
                for event in &transaction.events {
                    let typ = event.typ.to_string();
                    if self.events.contains(&typ) {
                        events.push(EndpointEvent {
                            address: event.guid.account_address.to_string(),
                            typ,
                            data: event.data.clone()
                        });
                    }
                }

                if !events.is_empty() {
                    request.transactions.push(EndpointTransaction {
                        version: transaction.info.version.0,
                        timestamp: transaction.timestamp.0,
                        events
                    });
                }
            }
        }

        /*let results = self
            .processor
            .process_transactions_with_status(transactions)
            .await;*/

        if !request.transactions.is_empty() {
            //TODO handle errors
            self.client.post(&self.endpoint).json(&request).send().await;
        }

        let batch_millis = (chrono::Utc::now().naive_utc() - batch_start).num_milliseconds();

        info!(
            num_txns = num_txns,
            time_millis = batch_millis,
            start_version = start_version,
            end_version = end_version,
            "Finished processing of transaction batch"
        );

        (num_txns, Some(Result::Ok(ProcessingResult {
            name: "",
            start_version: start_version.unwrap(),
            end_version: end_version.unwrap()
        })))
    }
}

pub async fn await_tasks<T: Debug>(tasks: Vec<JoinHandle<T>>) -> Vec<T> {
    let mut results = vec![];
    for task in tasks {
        let result = task.await;
        match result {
            Ok(_) => results.push(result.unwrap()),
            Err(err) => {
                panic!("Error joining task: {:?}", err);
            },
        }
    }
    results
}
