//! The real-time sender thread (workstream: real-time).
//!
//! Port of `_Sender._run` in `legacy/python/winpods/raop_live.py`, adapted to the
//! event-driven ring: loop { drain every complete 352-frame packet (with the
//! trim-after-stall branch); stall check; `wait_any([stop, ring_event,
//! hires_timer(4 ms)])` }. Silence only when the ring has had no writes for
//! `max(slack, 40 ms)` AND the timeline has run `slack` ahead, where
//! `slack = clamp(latency − render − 5, 5, 250)` ms; silence fills up to the
//! timeline (max 16 packets per wake); after a stall, trim ring content above
//! packet + 30 ms, up to the silence debt. MMCSS "Pro Audio"; no allocation,
//! logging or contended locks in the loop. Optional diagnostic `drop_every`:
//! skip the socket send of every Nth packet (still stored in the backlog).
