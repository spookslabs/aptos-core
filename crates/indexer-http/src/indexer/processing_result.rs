// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0
use serde::{Serialize, Deserialize};

#[derive(Debug)]
pub struct ProcessingResult {
    pub transactions: Vec<EndpointTransaction>,
    pub start_version: u64,
    pub end_version: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointRequest {
    pub transactions: Vec<EndpointTransaction>
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointTransaction {
    pub version: u64,
    pub timestamp: u64,
    pub events: Vec<EndpointEvent>,
    pub changes: Vec<EndpointTableChange>
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointEvent {
    pub address: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub data: serde_json::Value
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointTableChange {
    pub handle: String,
    pub key: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub value: Option<serde_json::Value>
}
