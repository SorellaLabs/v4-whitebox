use serde::{Deserialize, Serialize};
pub type Tick = i32;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickInfo {
    pub liquidity_net: i128,
    pub initialized:   bool
}
