//! Retransmission backlog (workstream: real-time).
//!
//! Preallocated 1024-slot ring of sent packets indexed by `seq & 1023` (each slot
//! stores seq, length and up to 1500 bytes). The sender stores every packet; the
//! control thread copies a slot out under a short lock and sends outside the lock.
//! Cleared per session.
//!
//! Every slot has its own lock, so the sender (writing the newest slot) and the
//! control thread (reading an old one) practically never contend; when they do,
//! the critical section is one ≤1500-byte copy.

use parking_lot::Mutex;

/// Number of packets kept (≈8 s of audio at 125 packets/s).
pub const BACKLOG_SLOTS: usize = 1024;
/// Largest packet a slot can hold.
pub const MAX_STORED_PACKET: usize = 1500;

struct Slot {
    seq: u16,
    /// 0 = empty.
    len: u16,
    data: [u8; MAX_STORED_PACKET],
}

impl Slot {
    const EMPTY: Slot = Slot {
        seq: 0,
        len: 0,
        data: [0; MAX_STORED_PACKET],
    };
}

/// Fixed-size store of recently sent packets, shared by the sender (writer) and the
/// control thread (reader).
pub struct Backlog {
    slots: Box<[Mutex<Slot>]>,
}

impl Default for Backlog {
    fn default() -> Self {
        Self::new()
    }
}

impl Backlog {
    /// Allocate all slots up front (≈1.5 MiB); nothing is allocated afterwards.
    pub fn new() -> Self {
        let slots = (0..BACKLOG_SLOTS)
            .map(|_| Mutex::new(Slot::EMPTY))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { slots }
    }

    fn slot(&self, seq: u16) -> &Mutex<Slot> {
        &self.slots[usize::from(seq) % BACKLOG_SLOTS]
    }

    /// Forget every stored packet.
    pub fn clear(&self) {
        for slot in self.slots.iter() {
            slot.lock().len = 0;
        }
    }

    /// Store the packet sent with sequence number `seq`, replacing whatever was in
    /// its slot. Returns false (and stores nothing) for empty or oversized packets.
    pub fn store(&self, seq: u16, packet: &[u8]) -> bool {
        if packet.is_empty() || packet.len() > MAX_STORED_PACKET {
            return false;
        }
        let mut slot = self.slot(seq).lock();
        slot.seq = seq;
        slot.len = packet.len() as u16;
        slot.data[..packet.len()].copy_from_slice(packet);
        true
    }

    /// Copy the packet with sequence number `seq` into the front of `out`. Returns
    /// its length, or `None` if it is no longer (or was never) stored, or `out` is
    /// too small.
    pub fn copy_out(&self, seq: u16, out: &mut [u8]) -> Option<usize> {
        let slot = self.slot(seq).lock();
        let len = usize::from(slot.len);
        if len == 0 || slot.seq != seq {
            return None;
        }
        out.get_mut(..len)?.copy_from_slice(&slot.data[..len]);
        Some(len)
    }

    /// Whether the packet with sequence number `seq` is stored.
    pub fn contains(&self, seq: u16) -> bool {
        let slot = self.slot(seq).lock();
        slot.len != 0 && slot.seq == seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_copy_out() {
        let backlog = Backlog::new();
        assert!(backlog.store(65534, &[1, 2, 3]));
        assert!(backlog.store(1, &[4; 1452]));
        let mut out = [0u8; MAX_STORED_PACKET];
        assert_eq!(backlog.copy_out(65534, &mut out), Some(3));
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(backlog.copy_out(1, &mut out), Some(1452));
        assert_eq!(backlog.copy_out(2, &mut out), None);
        assert!(backlog.contains(1));
        assert!(!backlog.contains(2));
        let mut small = [0u8; 2];
        assert_eq!(backlog.copy_out(65534, &mut small), None);
    }

    #[test]
    fn newer_packet_evicts_same_slot() {
        let backlog = Backlog::new();
        backlog.store(5, &[5]);
        backlog.store(5 + BACKLOG_SLOTS as u16, &[6]);
        let mut out = [0u8; 4];
        assert_eq!(backlog.copy_out(5, &mut out), None);
        assert_eq!(
            backlog.copy_out(5 + BACKLOG_SLOTS as u16, &mut out),
            Some(1)
        );
        assert_eq!(out[0], 6);
    }

    #[test]
    fn rejects_bad_sizes_and_clears() {
        let backlog = Backlog::new();
        assert!(!backlog.store(1, &[]));
        assert!(!backlog.store(1, &[0; MAX_STORED_PACKET + 1]));
        assert!(backlog.store(1, &[9; MAX_STORED_PACKET]));
        backlog.clear();
        assert!(!backlog.contains(1));
    }

    #[test]
    fn holds_the_last_1024_packets_across_wrap() {
        let backlog = Backlog::new();
        let mut seq = 65000u16;
        for _ in 0..3000 {
            backlog.store(seq, &seq.to_be_bytes());
            seq = seq.wrapping_add(1);
        }
        let mut out = [0u8; 2];
        let newest = seq.wrapping_sub(1);
        let oldest = seq.wrapping_sub(BACKLOG_SLOTS as u16);
        assert_eq!(backlog.copy_out(newest, &mut out), Some(2));
        assert_eq!(u16::from_be_bytes(out), newest);
        assert_eq!(backlog.copy_out(oldest, &mut out), Some(2));
        assert_eq!(backlog.copy_out(oldest.wrapping_sub(1), &mut out), None);
    }
}
