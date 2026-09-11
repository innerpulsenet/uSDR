//! Process-only key loading; never included in browser settings or diagnostics.
use anyhow::{Result, bail};
use scannerd_engine::crypto::{KeyStore, Protocol};
use serde::Deserialize;
use std::{io::Read, path::Path};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    keys: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    protocol: String,
    algorithm: u8,
    key_id: u16,
    key: String,
}
impl Drop for Entry {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.key.zeroize();
    }
}

pub fn load(path: &Path) -> Result<KeyStore> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("voice key file must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("voice key file must be private (chmod 600)");
        }
    }
    let mut text = Zeroizing::new(String::new());
    file.read_to_string(&mut text)?;
    parse(&text)
}

fn parse(text: &str) -> Result<KeyStore> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('{') {
        return parse_op25(text);
    }
    // Never display parser errors: they can contain the secret source text.
    let file: KeyFile =
        toml::from_str(text).map_err(|_| anyhow::anyhow!("invalid voice key file"))?;
    let mut keys = KeyStore::new();
    let mut seen = std::collections::HashSet::new();
    for entry in file.keys {
        let protocol = match entry.protocol.to_ascii_lowercase().as_str() {
            "p25" => Protocol::P25,
            "dmr" => Protocol::Dmr,
            _ => bail!("unknown voice key protocol"),
        };
        if !seen.insert((protocol, entry.algorithm, entry.key_id)) {
            bail!("duplicate voice key address");
        }
        let hex = entry.key.strip_prefix("0x").unwrap_or(&entry.key);
        if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("voice key must contain an even number of hexadecimal digits");
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        keys.insert(protocol, entry.algorithm, u32::from(entry.key_id), bytes)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(keys)
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(untagged)]
enum Number {
    Integer(u64),
    Text(String),
}
impl Number {
    fn value(&self) -> Result<u64> {
        match self {
            Self::Integer(n) => Ok(*n),
            Self::Text(s) => parse_number(s),
        }
    }
}
fn parse_number(s: &str) -> Result<u64> {
    let (digits, radix) = if let Some(v) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (v, 16)
    } else if let Some(v) = s.strip_prefix("0o") {
        (v, 8)
    } else if let Some(v) = s.strip_prefix("0b") {
        (v, 2)
    } else {
        (s, 10)
    };
    u64::from_str_radix(digits, radix).map_err(|_| anyhow::anyhow!("invalid voice key number"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Op25Entry {
    algid: Number,
    key: Vec<Number>,
}
fn parse_op25(text: &str) -> Result<KeyStore> {
    let entries: std::collections::BTreeMap<String, Op25Entry> =
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!("invalid OP25 voice key file"))?;
    let mut keys = KeyStore::new();
    let mut seen = std::collections::HashSet::new();
    for (id, entry) in entries {
        let id = u16::try_from(parse_number(&id)?)
            .map_err(|_| anyhow::anyhow!("key ID exceeds 16 bits"))?;
        let algorithm = u8::try_from(entry.algid.value()?)
            .map_err(|_| anyhow::anyhow!("algorithm exceeds 8 bits"))?;
        let size = match algorithm {
            0xaa => 5,
            0x81 => 8,
            0x84 => 32,
            _ => bail!("unsupported OP25 algorithm"),
        };
        if !seen.insert((algorithm, id)) {
            bail!("duplicate voice key address");
        }
        // Match OP25's leading-zero padding for short byte arrays. Reject
        // oversized values instead of silently truncating operator mistakes.
        if entry.key.is_empty() || entry.key.len() > size {
            bail!("wrong OP25 key length");
        }
        let mut bytes = Zeroizing::new(vec![0; size]);
        let offset = size - entry.key.len();
        for (i, value) in entry.key.iter().enumerate() {
            bytes[offset + i] = u8::try_from(value.value()?)
                .map_err(|_| anyhow::anyhow!("key byte exceeds 8 bits"))?;
        }
        keys.insert(
            Protocol::P25,
            algorithm,
            u32::from(id),
            std::mem::take(&mut *bytes),
        )
        .map_err(anyhow::Error::msg)?;
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn key_file_requires_private_permissions() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let path = std::env::temp_dir().join(format!("usdr-key-test-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"keys = []").unwrap();
        assert!(load(&path).unwrap().is_empty());
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn imports_boatbod_json_decimal_hex_and_short_keys() {
        assert_eq!(
            parse(r#"{"0x1234":{"algid":"0xaa","key":[1,"0x02",3]}}"#)
                .unwrap()
                .len(),
            1
        );
        assert!(parse(r#"{"1":{"algid":170,"key":[256]}}"#).is_err());
        assert!(parse(r#"{"65536":{"algid":170,"key":[1]}}"#).is_err());
    }
    #[test]
    fn accepts_exact_p25_addresses_and_leading_zero_keys() {
        let store =
            parse("[[keys]]\nprotocol='p25'\nalgorithm=0xaa\nkey_id=0x1234\nkey='0001020304'")
                .unwrap();
        assert_eq!(store.len(), 1);
    }
    #[test]
    fn rejects_bad_keys_without_echoing_secrets() {
        for key in ["SUPER_SECRET", "001", "00", "éé"] {
            let text = format!("[[keys]]\nprotocol='p25'\nalgorithm=170\nkey_id=1\nkey='{key}'");
            let error = parse(&text).err().unwrap().to_string();
            assert!(!error.contains(key));
        }
    }
}
