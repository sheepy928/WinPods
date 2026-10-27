//! Retransmission backlog (workstream: real-time).
//!
//! Preallocated 1024-slot ring of sent packets indexed by `seq & 1023` (each slot
//! stores seq, length and up to 1500 bytes). The sender stores every packet; the
//! control thread copies a slot out under a short lock and sends outside the lock.
//! Cleared per session.
