//! TLV8 encoding used by HAP pairing (workstream: control plane).
//!
//! Port of `pyatv/auth/hap_tlv8.py`: items are `tag(1) len(1) value`; values longer
//! than 255 bytes are split into consecutive items with the same tag and joined
//! again when decoding. Fixture: `tests/fixtures/tlv8.json`.
