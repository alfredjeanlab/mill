use bytesize::ByteSize;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serialize `ByteSize` as a u64 byte count for lossless round-tripping.
pub fn serialize<S: Serializer>(size: &ByteSize, s: S) -> Result<S::Ok, S::Error> {
    size.as_u64().serialize(s)
}

/// Deserialize `ByteSize` from a u64 byte count.
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ByteSize, D::Error> {
    let bytes = u64::deserialize(d)?;
    Ok(ByteSize(bytes))
}

/// For `Option<ByteSize>` fields.
pub mod option {
    use bytesize::ByteSize;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(size: &Option<ByteSize>, s: S) -> Result<S::Ok, S::Error> {
        match size {
            Some(bs) => s.serialize_some(&bs.as_u64()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ByteSize>, D::Error> {
        let opt = Option::<u64>::deserialize(d)?;
        Ok(opt.map(ByteSize))
    }
}
