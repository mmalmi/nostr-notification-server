//! Deliberately closed, identity-free diagnostics for encrypted call wakes.
use crate::notifications::EventPayload;
use nostr_sdk::Event;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(crate) struct CallDispatch(Option<String>);

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub(crate) enum Disposition {
    HttpAdmitted,
    Received,
    RelayFiltered,
    Stale,
    SignatureRejected,
    Matched { matches: usize },
    MatchFailed,
    Duplicate,
    ClaimFailed,
    FcmRateLimited,
    FcmQueued,
    FcmNotConfigured,
    FcmCredentialsFailed,
    FcmAuthFailed,
    FcmPayloadFailed,
    FcmTransportFailed,
    FcmAccepted { status: u16 },
    FcmRejected { status: u16 },
    DeliveryFailed,
}

impl CallDispatch {
    fn new(kind: u16, event_id: &str) -> Self {
        Self((kind == 21_111).then(|| {
            let mut hash = Sha256::new();
            hash.update(b"nns-call-dispatch-v1\0");
            hash.update(event_id.as_bytes());
            format!("{:x}", hash.finalize())
        }))
    }

    pub(crate) fn for_event(event: &Event) -> Self {
        Self::new(event.kind.as_u16(), &event.id.to_hex())
    }

    pub(crate) fn for_payload(event: &EventPayload) -> Self {
        match event {
            EventPayload::Full(event) => Self::for_event(event),
            EventPayload::Details(event) => Self::new(event.kind, &event.id),
        }
    }

    pub(crate) fn is_call(&self) -> bool {
        self.0.is_some()
    }

    fn record_json(&self, disposition: Disposition) -> Option<serde_json::Value> {
        self.0.as_ref().map(|fingerprint| {
            serde_json::json!({"call_wake_dispatch": {
                "fingerprint": fingerprint,
                "result": disposition,
            }})
        })
    }

    pub(crate) fn record(&self, disposition: Disposition) {
        if let Some(record) = self.record_json(disposition) {
            log::info!("{record}");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    struct DiagnosticLogger(Mutex<Vec<String>>);
    static LOGGER: DiagnosticLogger = DiagnosticLogger(Mutex::new(Vec::new()));
    static INIT: Once = Once::new();

    impl log::Log for DiagnosticLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if record.target().starts_with("nostr_notification_server") {
                self.0.lock().unwrap().push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    pub(crate) fn start_capture() {
        INIT.call_once(|| {
            log::set_logger(&LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    pub(crate) fn records(dispatch: &CallDispatch) -> Vec<serde_json::Value> {
        LOGGER
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| {
                record["call_wake_dispatch"]["fingerprint"].as_str() == dispatch.0.as_deref()
            })
            .collect()
    }

    pub(crate) fn assert_not_logged(secret: &str) {
        assert!(!LOGGER
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(secret)));
    }

    #[test]
    fn call_dispatch_records_only_closed_dispositions_and_opaque_fingerprints() {
        let secret = "sender recipient token subscription ciphertext provider-error";
        let call = CallDispatch::new(21_111, secret);
        assert_eq!(call.0, CallDispatch::new(21_111, secret).0);
        assert_ne!(call.0, CallDispatch::new(21_111, "another-event").0);
        for disposition in [
            Disposition::HttpAdmitted,
            Disposition::Received,
            Disposition::RelayFiltered,
            Disposition::Stale,
            Disposition::SignatureRejected,
            Disposition::Matched { matches: 0 },
            Disposition::Matched { matches: 2 },
            Disposition::MatchFailed,
            Disposition::Duplicate,
            Disposition::ClaimFailed,
            Disposition::FcmRateLimited,
            Disposition::FcmQueued,
            Disposition::FcmNotConfigured,
            Disposition::FcmCredentialsFailed,
            Disposition::FcmAuthFailed,
            Disposition::FcmPayloadFailed,
            Disposition::FcmTransportFailed,
            Disposition::FcmAccepted { status: 200 },
            Disposition::FcmRejected { status: 429 },
            Disposition::DeliveryFailed,
        ] {
            let record = call.record_json(disposition).unwrap();
            let fingerprint = record["call_wake_dispatch"]["fingerprint"]
                .as_str()
                .unwrap();
            assert_eq!(fingerprint.len(), 64);
            assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
            for word in secret.split_whitespace() {
                assert!(!record.to_string().contains(word));
            }
            assert!(CallDispatch::new(1060, secret)
                .record_json(disposition)
                .is_none());
        }
    }
}
