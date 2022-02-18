pub enum BeefyClientError {
    /// Failed to read a value from storage
    StorageReadError,
    /// Failed to write a value to storage
    StorageWriteError,
    /// Error decoding some value
    DecodingError,
    /// Invalid Mmr Update
    InvalidMmrUpdate,
    /// Error recovering public key from signature
    InvalidSignature,
    /// Some invalid merkle root hash
    InvalidRootHash,
}
