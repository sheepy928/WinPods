//! RTSP/HTTP request formatting, response parsing and the RTSP client
//! (workstream: control plane).
//!
//! Byte-exact port of `pyatv/support/http.py` + `pyatv/support/rtsp.py` as used by
//! WinPods: CSeq starts at 0 (GET /info takes CSeq 0); headers CSeq, DACP-ID (u64
//! uppercase hex), Active-Remote (u32), Client-Instance (= DACP-ID), then extra
//! headers; `User-Agent: AirPlay/550.10`; URI `rtsp://<local ip>/<session id>`;
//! dict bodies are binary plists with `Content-Type: application/x-apple-binary-plist`;
//! 4 s response timeout. Pairing posts use HTTP/1.1 and no CSeq. Once pairing
//! completes, every byte in both directions goes through the HAP cipher.
//! Fixture: `tests/fixtures/rtsp_requests.json`.
