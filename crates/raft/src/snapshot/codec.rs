use crate::error::SnapshotError;
use crate::fsm::FsmState;

/// Encode FSM state as MessagePack + zstd compressed bytes.
pub fn encode(state: &FsmState) -> Result<Vec<u8>, SnapshotError> {
    let msgpack = rmp_serde::to_vec(state).map_err(|e| SnapshotError::Encode(e.to_string()))?;
    zstd::encode_all(msgpack.as_slice(), 3).map_err(|e| SnapshotError::Encode(e.to_string()))
}

/// Decode zstd-compressed MessagePack bytes into FSM state.
pub fn decode(data: &[u8]) -> Result<FsmState, SnapshotError> {
    let msgpack = zstd::decode_all(data).map_err(|e| SnapshotError::Decode(e.to_string()))?;
    rmp_serde::from_slice(&msgpack).map_err(|e| SnapshotError::Decode(e.to_string()))
}
