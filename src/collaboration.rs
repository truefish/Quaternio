pub mod document;
pub mod audit;
pub mod peer;
pub mod session;
pub mod outbox;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OperationId { pub author: String, pub sequence: u64 }
