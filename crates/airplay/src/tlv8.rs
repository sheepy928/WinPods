//! TLV8 encoding used by HAP pairing (workstream: control plane).
//!
//! Port of `pyatv/auth/hap_tlv8.py`: items are `tag(1) len(1) value`; values longer
//! than 255 bytes are split into consecutive items with the same tag and joined
//! again when decoding. Fixture: `tests/fixtures/tlv8.json`.
//!
//! Two pyatv behaviours are kept on purpose: [`encode`] emits nothing for an empty
//! value, and [`decode`] concatenates every occurrence of a tag (not only adjacent
//! fragments).

/// Tag numbers from the HAP specification (`TlvValue` in pyatv).
pub mod tag {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const SEQ_NO: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const BACK_OFF: u8 = 0x08;
    pub const SIGNATURE: u8 = 0x0A;
    pub const NAME: u8 = 0x11;
    pub const FLAGS: u8 = 0x13;
}

/// Largest value a single TLV8 item can carry.
const MAX_ITEM: usize = 255;

/// Decoding failed because an item runs past the end of the input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("TLV8 item with tag {tag:#04x} is truncated at offset {offset}")]
pub struct Tlv8Error {
    pub tag: u8,
    pub offset: usize,
}

/// Encode `(tag, value)` items in order, splitting values longer than 255 bytes.
///
/// Like pyatv's `write_tlv`, an empty value produces no bytes at all.
pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let total: usize = items
        .iter()
        .map(|(_, value)| value.len() + 2 * value.len().div_ceil(MAX_ITEM))
        .sum();
    let mut out = Vec::with_capacity(total);
    for &(tag, value) in items {
        for chunk in value.chunks(MAX_ITEM) {
            out.push(tag);
            // `chunks(255)` guarantees the length fits in a byte.
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Decoded TLV8 items: one entry per tag, in order of first appearance, with the
/// values of repeated tags concatenated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tlv8 {
    items: Vec<(u8, Vec<u8>)>,
}

impl Tlv8 {
    /// Value of `tag`, if present (possibly empty).
    pub fn get(&self, tag: u8) -> Option<&[u8]> {
        self.items
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, value)| value.as_slice())
    }

    /// First byte of `tag`'s value (SeqNo, Error, Method...).
    pub fn get_u8(&self, tag: u8) -> Option<u8> {
        self.get(tag).and_then(|value| value.first().copied())
    }

    /// All `(tag, value)` pairs in order of first appearance.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &[u8])> {
        self.items.iter().map(|(t, v)| (*t, v.as_slice()))
    }

    /// Number of distinct tags.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when no item was decoded.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Decode TLV8 bytes, concatenating repeated tags like pyatv's `read_tlv`.
pub fn decode(data: &[u8]) -> Result<Tlv8, Tlv8Error> {
    let mut out = Tlv8::default();
    let mut pos = 0;
    while pos < data.len() {
        let tag = data[pos];
        let len = *data.get(pos + 1).ok_or(Tlv8Error { tag, offset: pos })? as usize;
        let value = data
            .get(pos + 2..pos + 2 + len)
            .ok_or(Tlv8Error { tag, offset: pos })?;
        match out.items.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, existing)) => existing.extend_from_slice(value),
            None => out.items.push((tag, value.to_vec())),
        }
        pos += 2 + len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn fixture() -> Json {
        serde_json::from_str(include_str!("../tests/fixtures/tlv8.json")).expect("tlv8.json")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    #[test]
    fn m1_matches_pyatv() {
        let fx = fixture();
        let m1 = encode(&[
            (tag::METHOD, &[0x00]),
            (tag::SEQ_NO, &[0x01]),
            (tag::FLAGS, &[0x10]),
        ]);
        assert_eq!(m1, unhex(&fx["m1"]));
    }

    #[test]
    fn fragmented_value_round_trips() {
        let fx = fixture();
        let frag = &fx["fragmented"];
        let tag = frag["tag"].as_u64().expect("tag") as u8;
        let value = unhex(&frag["value"]);
        let encoded = encode(&[(tag, &value)]);
        assert_eq!(encoded, unhex(&frag["encoded"]));
        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded.get(tag), Some(value.as_slice()));
        assert_eq!(decoded.len(), 1);
    }

    #[test]
    fn decode_cases_match_pyatv() {
        let fx = fixture();
        for case in fx["decode_cases"].as_array().expect("cases") {
            let name = case["name"].as_str().expect("name");
            let decoded = decode(&unhex(&case["encoded"])).expect(name);
            let expected = case["decoded"].as_object().expect("decoded");
            assert_eq!(decoded.len(), expected.len(), "{name}: tag count");
            for (key, value) in expected {
                let tag: u8 = key.parse().expect("decimal tag");
                assert_eq!(
                    decoded.get(tag),
                    Some(unhex(value).as_slice()),
                    "{name}: tag {tag}"
                );
            }
        }
    }

    #[test]
    fn encoding_decoded_cases_reproduces_input_except_empty_values() {
        let fx = fixture();
        for case in fx["decode_cases"].as_array().expect("cases") {
            let name = case["name"].as_str().expect("name");
            let encoded = unhex(&case["encoded"]);
            let decoded = decode(&encoded).expect(name);
            let items: Vec<(u8, &[u8])> = decoded.iter().collect();
            let again = encode(&items);
            if name == "empty_value_item" {
                // pyatv's write_tlv drops empty values.
                assert_eq!(again, vec![0x06, 0x01, 0x02], "{name}");
            } else {
                assert_eq!(again, encoded, "{name}");
            }
        }
    }

    #[test]
    fn empty_value_encodes_to_nothing() {
        assert!(encode(&[(tag::ERROR, &[])]).is_empty());
    }

    #[test]
    fn repeated_non_adjacent_tags_are_concatenated() {
        let decoded = decode(&[0x03, 0x01, 0xAA, 0x06, 0x01, 0x02, 0x03, 0x01, 0xBB]).unwrap();
        assert_eq!(decoded.get(tag::PUBLIC_KEY), Some(&[0xAA, 0xBB][..]));
        assert_eq!(decoded.get_u8(tag::SEQ_NO), Some(2));
    }

    #[test]
    fn truncated_input_is_an_error() {
        assert_eq!(
            decode(&[0x06, 0x01, 0x02, 0x07]),
            Err(Tlv8Error { tag: 7, offset: 3 })
        );
        assert_eq!(
            decode(&[0x04, 0x05, 0x01, 0x02]),
            Err(Tlv8Error { tag: 4, offset: 0 })
        );
        assert!(decode(&[]).expect("empty").is_empty());
    }
}
