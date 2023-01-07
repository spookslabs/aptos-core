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
use std::{fmt::Debug, sync::Arc};
use tokio::{sync::Mutex, task::JoinHandle};
use aptos_api_types::{Transaction, WriteSetChange};
use crate::indexer::processing_result::{EndpointTableChange, EndpointTransaction, EndpointEvent, EndpointResourceChange};

#[derive(Clone)]
pub struct Tailer {
    pub transaction_fetcher: Arc<Mutex<dyn TransactionFetcherTrait>>,
    events: Vec<String>,
    resources: Vec<String>,
    handles: Vec<String>
}

impl Tailer {
    pub fn new(
        context: Arc<ApiContext>,
        events: Vec<String>,
        resources: Vec<String>,
        handles: Vec<String>,
        options: TransactionFetcherOptions,
    ) -> Result<Tailer, ParseError> {
        let resolver = Arc::new(context.move_resolver().unwrap());
        let transaction_fetcher = TransactionFetcher::new(context, resolver, 0, options);

        Ok(Self {
            transaction_fetcher: Arc::new(Mutex::new(transaction_fetcher)),
            events,
            resources,
            handles
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
        Option<ProcessingResult>,
    ) {
        let raw_transactions = self
            .transaction_fetcher
            .lock()
            .await
            .fetch_next_batch()
            .await;

        let num_txns = raw_transactions.len() as u64;
        // When the batch is empty b/c we're caught up
        if num_txns == 0 {
            return (0, None);
        }
        let start_version = raw_transactions.first().unwrap().version();
        let end_version = raw_transactions.last().unwrap().version();

        debug!(
            num_txns = num_txns,
            start_version = start_version,
            end_version = end_version,
            "Starting processing of transaction batch"
        );

        let batch_start = chrono::Utc::now().naive_utc();

        let mut transactions = vec![];

        for transaction in &raw_transactions {
            if let Transaction::UserTransaction(transaction) = transaction {
                if transaction.info.success {

                    let mut events = vec![];
                    let mut resources = vec![];
                    let mut changes = vec![];

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

                    for write in &transaction.info.changes {
                        match write {
                            WriteSetChange::DeleteTableItem(item) => {
                                let handle = item.handle.to_string();
                                if self.handles.contains(&handle) {
                                    changes.push(EndpointTableChange {
                                        handle,
                                        key: item.data.as_ref().unwrap().key.clone(),
                                        value: None
                                    });
                                }
                            }
                            WriteSetChange::DeleteResource(resource) => {
                                let typ = resource.resource.to_string();
                                if self.resources.contains(&typ) {
                                    resources.push(EndpointResourceChange {
                                        address: resource.address.to_string(),
                                        typ,
                                        data: None
                                    });
                                }
                            }
                            WriteSetChange::WriteTableItem(item) => {
                                let handle = item.handle.to_string();
                                if self.handles.contains(&handle) {
                                    let data = item.data.as_ref().unwrap();
                                    changes.push(EndpointTableChange {
                                        handle,
                                        key: data.key.clone(),
                                        value: Some(data.value.clone())
                                    });
                                }
                            }
                            WriteSetChange::WriteResource(resource) => {
                                let typ = resource.data.typ.to_string();
                                if self.resources.contains(&typ) {
                                    resources.push(EndpointResourceChange {
                                        address: resource.address.to_string(),
                                        typ,
                                        data: Some(resource.data.data.clone())
                                    });
                                }
                            }
                            _ => {}
                        }
                    }

                    if !events.is_empty() || !resources.is_empty() || !changes.is_empty() {
                        transactions.push(EndpointTransaction {
                            version: transaction.info.version.0,
                            timestamp: transaction.timestamp.0,
                            events,
                            resources,
                            changes
                        });
                    }
                }
            }
        }

        let batch_millis = (chrono::Utc::now().naive_utc() - batch_start).num_milliseconds();

        info!(
            num_txns = num_txns,
            time_millis = batch_millis,
            start_version = start_version,
            end_version = end_version,
            "Finished processing of transaction batch"
        );

        (num_txns, Some(ProcessingResult {
            transactions,
            start_version: start_version.unwrap(),
            end_version: end_version.unwrap()
        }))
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
