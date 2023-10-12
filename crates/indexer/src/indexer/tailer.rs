// Copyright © Aptos Foundation

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
use crate::indexer::processing_result::{EndpointTableChange, EndpointTransaction, EndpointEvent, EndpointResourceChange};
use aptos_protos::transaction::v1::{Transaction, WriteSetChange};
use aptos_protos::transaction::v1::write_set_change::Change;
use aptos_protos::transaction::v1::transaction::TransactionType::User;
use aptos_protos::transaction::v1::transaction::TxnData;
use std::ops::{Div, Mul, Add};

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
        let transaction_fetcher = TransactionFetcher::new(context, 0, options);

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

    pub fn process(&self, raw_transactions: &Vec<Transaction>) -> Vec<EndpointTransaction> {

        let mut transactions = vec![];

        for transaction in raw_transactions {
            if let Some(TxnData::User(user_transaction)) = &transaction.txn_data {

                let info = transaction.info.as_ref().unwrap();

                if info.success {

                    let mut events = vec![];
                    let mut resources = vec![];
                    let mut changes = vec![];

                    for event in &user_transaction.events {
                        let typ = event.type_str.to_string();
                        if self.events.contains(&typ) {
                            events.push(EndpointEvent {
                                address: event.key.as_ref().unwrap().account_address.clone(),
                                typ,
                                data: serde_json::from_str(event.data.as_ref()).unwrap()
                            });
                        }
                    }

                    for write in &info.changes {
                        match write.change.as_ref().unwrap() {
                            Change::DeleteTableItem(item) => {
                                let handle = item.handle.to_string();
                                if self.handles.contains(&handle) {
                                    changes.push(EndpointTableChange {
                                        handle,
                                        key: serde_json::from_str(item.key.as_ref()).unwrap(),
                                        value: None
                                    });
                                }
                            }
                            Change::DeleteResource(resource) => {
                                let typ = resource.type_str.to_string();
                                if self.resources.contains(&typ) {
                                    resources.push(EndpointResourceChange {
                                        address: resource.address.to_string(),
                                        typ,
                                        data: None
                                    });
                                }
                            }
                            Change::WriteTableItem(item) => {
                                let handle = item.handle.to_string();
                                if self.handles.contains(&handle) {
                                    let data = item.data.as_ref().unwrap();
                                    changes.push(EndpointTableChange {
                                        handle,
                                        key: serde_json::from_str(data.key.as_ref()).unwrap(),
                                        value: Some(serde_json::from_str(data.value.as_ref()).unwrap())
                                    });
                                }
                            }
                            Change::WriteResource(resource) => {
                                let typ = resource.type_str.to_string();
                                if self.resources.contains(&typ) {
                                    resources.push(EndpointResourceChange {
                                        address: resource.address.to_string(),
                                        typ,
                                        data: Some(serde_json::from_str(&resource.data).unwrap())
                                    });
                                }
                            }
                            _ => {}
                        }
                    }

                    if !events.is_empty() || !resources.is_empty() || !changes.is_empty() {

                        let timestamp = transaction.timestamp.clone().unwrap();

                        transactions.push(EndpointTransaction {
                            version: transaction.version,
                            timestamp: timestamp.seconds as u64 * 1000000 + timestamp.nanos as u64 / 1000,
                            events,
                            resources,
                            changes
                        });
                    }
                }
            }
        }

        transactions
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
