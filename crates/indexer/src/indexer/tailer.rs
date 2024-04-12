// Copyright © Aptos Foundation

// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0
use crate::{
    indexer::{
        fetcher::{TransactionFetcher, TransactionFetcherOptions, TransactionFetcherTrait},
        processing_result::ProcessingResult
    }
};
use anyhow::{Context, Result};
use aptos_api::context::Context as ApiContext;
use aptos_logger::{debug, info};
use chrono::ParseError;
use std::{fmt::Debug, sync::Arc};
use std::str::FromStr;
use tokio::{sync::Mutex, task::JoinHandle};
use aptos_api_types::{Transaction, WriteSetChange, TransactionPayload, Address, IdentifierWrapper, MoveType, MoveStructTag};
use aptos_types::account_address::AccountAddress;
use crate::indexer::processing_result::{EndpointTableChange, EndpointTransaction, EndpointEvent, EndpointResourceChange};

#[derive(Clone)]
pub struct Type {
    pub address: Address,
    pub module: IdentifierWrapper,
    pub name: IdentifierWrapper,
}

impl Type {

    pub fn new(address: Address, module: IdentifierWrapper, name: IdentifierWrapper) -> Type {
        Self {
            address,
            module,
            name
        }
    }

    pub fn eq(&self, struct_tag: &MoveStructTag) -> bool {
        self.address == struct_tag.address && self.module == struct_tag.module && self.name == struct_tag.name
    }

}

#[derive(Clone)]
struct TypeVec(Vec<Type>);

impl TypeVec {

    pub fn contains_move_type(&self, move_type: &MoveType) -> bool {
        if let MoveType::Struct(s) = move_type {
            self.contains(&s)
        } else {
            false
        }
    }

    pub fn contains(&self, struct_tag: &MoveStructTag) -> bool {
        self.0.iter().any(|typ| typ.eq(&struct_tag))
    }

}

fn to_type(types: &Vec<String>) -> TypeVec {
    TypeVec(types.iter().map(|str| {
        let split: Vec<&str> = str.split("::").collect();
        if split.len() != 3 {
            panic!("Invalid length");
        }
        Type::new(
            Address::from(AccountAddress::from_hex_literal(&split[0]).unwrap()),
            IdentifierWrapper::from_str(&split[1]).unwrap(),
            IdentifierWrapper::from_str(&split[2]).unwrap()
        )
    }).collect())
}

#[derive(Clone)]
pub struct Tailer {
    pub transaction_fetcher: Arc<Mutex<dyn TransactionFetcherTrait>>,
    events: TypeVec,
    resources: TypeVec,
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
            events: to_type(&events),
            resources: to_type(&resources),
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
                        if self.events.contains_move_type(&event.typ) {
                            events.push(EndpointEvent {
                                address: event.guid.account_address.to_string(),
                                typ: event.typ.to_string(),
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
                                if self.resources.contains(&resource.resource) {
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
                                if self.resources.contains(&resource.data.typ) {
                                    resources.push(EndpointResourceChange {
                                        address: resource.address.to_string(),
                                        typ: resource.data.typ.to_string(),
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
                            function: if let TransactionPayload::EntryFunctionPayload(payload) = &transaction.request.payload {
                                Some(payload.function.to_string())
                            } else {
                                None
                            },
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
