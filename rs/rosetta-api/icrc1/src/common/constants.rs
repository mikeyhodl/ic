use std::time::Duration;

pub const DEFAULT_BLOCKCHAIN: &str = "Internet Computer";
pub const ROSETTA_VERSION: &str = "1.2.4";
pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const INGRESS_INTERVAL_SECS: u64 = 4 * 60;
pub const BLOCK_SYNC_WAIT_SECS: u64 = 1;
pub const MAX_BLOCK_SYNC_WAIT_SECS: u64 = 60;
pub const MIN_PROGRESS_BAR: u64 = 50;
pub const MINT_OPERATION_IDENTIFIER: u64 = 0;
pub const BURN_OPERATION_IDENTIFIER: u64 = 1;
pub const TRANSFER_TO_OPERATION_IDENTIFIER: u64 = 3;
pub const TRANSFER_FROM_OPERATION_IDENTIFIER: u64 = 4;
pub const FEE_OPERATION_IDENTIFIER: u64 = 5;
pub const APPROVE_OPERATION_IDENTIFIER: u64 = 6;
pub const SPENDER_OPERATION_IDENTIFIER: u64 = 7;
pub const FEE_COLLECTOR_OPERATION_IDENTIFIER: u64 = 8;
pub const MAX_TRANSACTIONS_PER_SEARCH_TRANSACTIONS_REQUEST: u64 = 10000;
pub const INGRESS_INTERVAL_OVERLAP: Duration = Duration::from_secs(120);
pub const STATUS_COMPLETED: &str = "COMPLETED";
pub const MAX_BLOCKS_PER_QUERY_BLOCK_RANGE_REQUEST: u64 = 10000;
