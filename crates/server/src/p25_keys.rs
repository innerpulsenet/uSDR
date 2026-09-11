//! Write-only browser key entry and private channel-scoped persistence.
use crate::{ApiError, AppState};
use axum::{
    Json,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use scannerd_engine::crypto::{self, KeyStore, Protocol};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedKey {
    algorithm: u8,
    key_id: u16,
    key: String,
}
impl Drop for SavedKey {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    frequency_hz: u64,
    action: String,
    entry: Option<SavedKey>,
}
#[derive(Deserialize)]
pub struct Frequency {
    frequency_hz: u64,
}
#[derive(Serialize)]
pub struct Status {
    frequency_hz: u64,
    configured: bool,
    algorithm: Option<u8>,
    key_id: Option<u16>,
    /// Safe inventory of channel-scoped keys. Key material never crosses the
    /// API; frequency + ALGID + KID are enough to match an over-the-air call.
    entries: Vec<KeySummary>,
}
#[derive(Serialize)]
struct KeySummary {
    frequency_hz: u64,
    algorithm: u8,
    key_id: u16,
    key_bits: u16,
}

pub struct ChannelKeys {
    path: PathBuf,
    entries: BTreeMap<u64, SavedKey>,
}
fn validate_frequency(hz: u64) -> Result<(), &'static str> {
    if !(1..=10_000_000_000).contains(&hz) {
        return Err("invalid P25 channel frequency");
    }
    Ok(())
}
fn key_bytes(entry: &SavedKey) -> Result<Vec<u8>, &'static str> {
    let hex = entry.key.trim();
    let hex = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    let size = match entry.algorithm {
        0xaa => 5,
        0x81 => 8,
        0x84 => 32,
        0x89 => 16,
        _ => return Err("unsupported P25 key type"),
    };
    if hex.len() != size * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("key must be full-length hexadecimal, including leading zeros");
    }
    Ok((0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect())
}
impl ChannelKeys {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let entries: BTreeMap<u64, SavedKey> = match std::fs::File::open(&path) {
            Ok(mut file) => {
                use std::io::Read;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    anyhow::ensure!(
                        file.metadata()?.permissions().mode() & 0o077 == 0,
                        "P25 channel key file must be private (chmod 600)"
                    );
                }
                let mut text = Zeroizing::new(String::new());
                file.read_to_string(&mut text)?;
                serde_json::from_str(&text)
                    .map_err(|_| anyhow::anyhow!("invalid P25 channel key file"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        for (frequency, entry) in &entries {
            validate_frequency(*frequency).map_err(anyhow::Error::msg)?;
            let mut check = KeyStore::new();
            check
                .insert(
                    Protocol::P25,
                    entry.algorithm,
                    u32::from(entry.key_id),
                    key_bytes(entry).map_err(anyhow::Error::msg)?,
                )
                .map_err(anyhow::Error::msg)?;
        }
        for (frequency, entry) in &entries {
            crypto::set_p25_channel_key(
                *frequency,
                entry.algorithm,
                entry.key_id,
                key_bytes(entry).map_err(anyhow::Error::msg)?,
            )
            .map_err(anyhow::Error::msg)?;
        }
        Ok(Self { path, entries })
    }
    fn status(&self, frequency_hz: u64) -> Status {
        let entry = self.entries.get(&frequency_hz);
        Status {
            frequency_hz,
            configured: entry.is_some(),
            algorithm: entry.map(|e| e.algorithm),
            key_id: entry.map(|e| e.key_id),
            entries: self
                .entries
                .iter()
                .map(|(&frequency_hz, entry)| KeySummary {
                    frequency_hz,
                    algorithm: entry.algorithm,
                    key_id: entry.key_id,
                    key_bits: match entry.algorithm {
                        0xaa => 40,
                        0x81 => 64,
                        0x84 => 256,
                        0x89 => 128,
                        _ => 0,
                    },
                })
                .collect(),
        }
    }
    fn apply(&mut self, request: Request) -> Result<Status, ApiError> {
        validate_frequency(request.frequency_hz).map_err(|s| ApiError::BadRequest(s.into()))?;
        let mut next = self.entries.clone();
        let mut bytes = Zeroizing::new(Vec::new());
        match request.action.as_str() {
            "save" => {
                let entry = request.entry.ok_or_else(|| {
                    ApiError::BadRequest("key type, key ID and key are required".into())
                })?;
                *bytes = key_bytes(&entry).map_err(|s| ApiError::BadRequest(s.into()))?;
                next.insert(request.frequency_hz, entry);
            }
            "remove" => {
                next.remove(&request.frequency_hz);
            }
            _ => return Err(ApiError::BadRequest("action must be save or remove".into())),
        }
        self.persist(&next)?;
        if let Some(entry) = next.get(&request.frequency_hz) {
            crypto::set_p25_channel_key(
                request.frequency_hz,
                entry.algorithm,
                entry.key_id,
                std::mem::take(&mut *bytes),
            )
            .map_err(|_| ApiError::BadRequest("invalid P25 key".into()))?;
        } else {
            crypto::remove_p25_channel_key(request.frequency_hz);
        }
        self.entries = next;
        Ok(self.status(request.frequency_hz))
    }
    fn persist(&self, entries: &BTreeMap<u64, SavedKey>) -> anyhow::Result<()> {
        let parent = self.path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let temp = self.path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let result = (|| -> anyhow::Result<()> {
            let data = Zeroizing::new(
                serde_json::to_vec(entries)
                    .map_err(|_| anyhow::anyhow!("could not encode P25 keys"))?,
            );
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Ok(origin) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    matches!(origin.scheme_str(), Some("http" | "https"))
        && origin.authority().is_some_and(|a| a.as_str() == host)
}
fn no_store(status: Status) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(status)).into_response()
}
pub async fn get(
    State(st): State<Arc<AppState>>,
    Query(query): Query<Frequency>,
) -> Result<Response, ApiError> {
    validate_frequency(query.frequency_hz).map_err(|s| ApiError::BadRequest(s.into()))?;
    Ok(no_store(
        st.p25_keys
            .lock()
            .expect("P25 keys")
            .status(query.frequency_hz),
    ))
}
pub async fn post(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !same_origin(&headers) {
        return Err(ApiError::BadRequest(
            "P25 key changes must come from this web interface".into(),
        ));
    }
    let body = Zeroizing::new(body.to_vec());
    let request: Request = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("invalid P25 key request".into()))?;
    Ok(no_store(
        st.p25_keys.lock().expect("P25 keys").apply(request)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(frequency_hz: u64, key: &str) -> Request {
        Request {
            frequency_hz,
            action: "save".into(),
            entry: Some(SavedKey {
                algorithm: 0xaa,
                key_id: 0xbeef,
                key: key.into(),
            }),
        }
    }
    fn stream(frequency: u64) -> Option<[u8; 11]> {
        let mut decoder = crypto::VoiceDecryptor::for_p25_phase1_on_channel(
            Some(frequency as f64),
            0xaa,
            0xbeef,
            &[1; 9],
            0,
        )?;
        let mut word = [0; 11];
        decoder.apply_imbe88(&mut word);
        Some(word)
    }
    #[test]
    fn write_only_keys_persist_replace_and_remove_without_restarting() {
        let hz = 913127251;
        let dir = std::env::temp_dir().join(format!("usdr-web-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("private.json");
        let mut store = ChannelKeys::load(path.clone()).unwrap();
        let response = store.apply(request(hz, "0001020304")).ok().unwrap();
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["configured"], true);
        assert_eq!(json["entries"][0]["frequency_hz"], hz);
        assert_eq!(json["entries"][0]["algorithm"], 0xaa);
        assert_eq!(json["entries"][0]["key_id"], 0xbeef);
        assert_eq!(json["entries"][0]["key_bits"], 40);
        assert!(json.get("key").is_none());
        assert!(!json.to_string().contains("0001020304"));
        let first = stream(hz).unwrap();
        assert!(
            stream(hz + 1).is_none(),
            "keys must not leak to another frequency"
        );
        assert!(store.apply(request(hz, "123")).is_err());
        assert_eq!(
            stream(hz),
            Some(first),
            "invalid replacement must keep working key"
        );
        store.apply(request(hz, "0102030405")).ok().unwrap();
        let replacement = stream(hz).unwrap();
        assert_ne!(first, replacement);
        crypto::remove_p25_channel_key(hz);
        assert!(stream(hz).is_none());
        let mut reloaded = ChannelKeys::load(path.clone()).unwrap();
        assert_eq!(stream(hz), Some(replacement));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        reloaded
            .apply(Request {
                frequency_hz: hz,
                action: "remove".into(),
                entry: None,
            })
            .ok()
            .unwrap();
        assert!(stream(hz).is_none());
        assert!(!ChannelKeys::load(path).unwrap().status(hz).configured);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_disk_write_does_not_install_a_key() {
        let hz = 913127252;
        let dir = std::env::temp_dir().join(format!("usdr-web-key-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let parent = dir.join("file");
        std::fs::write(&parent, b"not a directory").unwrap();
        let mut store = ChannelKeys {
            path: parent.join("key.json"),
            entries: BTreeMap::new(),
        };
        assert!(store.apply(request(hz, "0001020304")).is_err());
        assert!(!store.status(hz).configured);
        assert!(stream(hz).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn request_contract_and_origin_check() {
        let request:Request = serde_json::from_str(r#"{"frequency_hz":851000000,"action":"save","entry":{"algorithm":170,"key_id":1,"key":"0001020304"}}"#).unwrap();
        assert_eq!(
            key_bytes(request.entry.as_ref().unwrap()).unwrap(),
            [0, 1, 2, 3, 4]
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8073".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "https://unrelated.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "http://127.0.0.1:8073".parse().unwrap());
        assert!(same_origin(&headers));
    }
}
