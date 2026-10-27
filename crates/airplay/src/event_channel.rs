//! Event channel (workstream: control plane).
//!
//! TCP connection to the `eventPort` from the base SETUP response, HAP-framed with
//! the event keys (send = Events-Read key, receive = Events-Write key). The receiver
//! sends `POST /command RTSP/1.0` requests (~2 KB binary plists, spanning several
//! frames/reads); reply once per complete request with
//! `<proto>/<ver> 200 OK`, `Content-Length: 0`, `Audio-Latency: 0`, the request's
//! `Server` header echoed (else `WinPods/<version>`) and its `CSeq` echoed.
//! Parse errors are logged and the channel stays open. Port of
//! `pyatv/protocols/airplay/channels.py` EventChannel. Fixture: `event_channel.json`.
