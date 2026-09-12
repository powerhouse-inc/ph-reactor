//! Drive configuration and the status vocabulary.
//!
//! A drive is a remote **peer**: a multiaddr (optionally with a `/p2p/`
//! component) of another ph-reactor instance. Sync is bidirectional
//! libp2p (Noise-encrypted, key-identified); there is no central server.
//! The runtime state of a drive (connection, backoff, catch-up) lives in
//! the p2p engine; this module holds the persisted shape and the
//! status vocabulary of the `status` contract.

use std::str::FromStr;

use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};

/// Drive status vocabulary for the `status` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriveStatus {
    /// Connected and fully up to date.
    Synced,
    /// Dial, handshake, or catch-up in progress.
    Connecting,
    /// Paused by configuration.
    Paused,
    /// Not connected and no recent contact.
    Offline,
    /// Handshake rejected: a token is required.
    RequiresAuth,
    /// Transport, protocol, or handshake error.
    Error,
}

impl DriveStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DriveStatus::Synced => "synced",
            DriveStatus::Connecting => "connecting",
            DriveStatus::Paused => "paused",
            DriveStatus::Offline => "offline",
            DriveStatus::RequiresAuth => "requires-auth",
            DriveStatus::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "synced" => Some(Self::Synced),
            "connecting" => Some(Self::Connecting),
            "paused" => Some(Self::Paused),
            "offline" => Some(Self::Offline),
            "requires-auth" => Some(Self::RequiresAuth),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

mod multiaddr_serde {
    use super::*;
    pub fn serialize<S: serde::Serializer>(m: &Multiaddr, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&m.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Multiaddr, D::Error> {
        let s = String::deserialize(d)?;
        Multiaddr::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// A configured remote drive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub name: String,
    /// Peer address, e.g.
    /// `/ip4/10.0.0.2/tcp/4201/p2p/12D3Koo...` or
    /// `/ip4/10.0.0.2/tcp/4201` (peer resolved on connect).
    #[serde(with = "multiaddr_serde")]
    pub addr: Multiaddr,
    /// Name of the environment variable holding the shared token.
    /// Only the name is persisted; the value is read from the
    /// environment at connection time.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default = "default_true")]
    pub available_offline: bool,
}

fn default_true() -> bool {
    true
}

impl Drive {
    /// The shared token value, resolved from the environment (None when
    /// unset or the variable is empty).
    pub fn token(&self) -> Option<String> {
        self.token_env
            .as_deref()
            .and_then(|n| std::env::var(n).ok())
            .filter(|v| !v.is_empty())
    }

    /// The `/p2p/` peer-id component of the multiaddr, if present.
    pub fn peer_id(&self) -> Option<libp2p::PeerId> {
        use libp2p::multiaddr::Protocol;
        self.addr.iter().find_map(|p| match p {
            Protocol::P2p(pid) => Some(pid),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(addr: &str) -> Drive {
        Drive {
            name: "vault".into(),
            addr: addr.parse().unwrap(),
            token_env: Some("VAULT_TOKEN".into()),
            paused: false,
            available_offline: true,
        }
    }

    #[test]
    fn drive_serializes_the_multiaddr_as_a_string() {
        let addr = format!("/ip4/10.0.0.2/tcp/4201/p2p/{}", crate::p2p::test_peer_id());
        let d = drive(&addr);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(&format!("\"addr\":\"{addr}\"")));
        let back: Drive = serde_json::from_str(&json).unwrap();
        assert_eq!(back.addr, d.addr);
        assert_eq!(back.peer_id(), d.peer_id());
    }

    #[test]
    fn peer_id_only_when_present() {
        let addr = format!("/ip4/127.0.0.1/tcp/4201/p2p/{}", crate::p2p::test_peer_id());
        let with = drive(&addr);
        assert!(with.peer_id().is_some());
        let without = drive("/ip4/127.0.0.1/tcp/4201");
        assert!(without.peer_id().is_none());
    }

    #[test]
    fn status_vocabulary_round_trips() {
        for s in [
            "synced",
            "connecting",
            "paused",
            "offline",
            "requires-auth",
            "error",
        ] {
            let st = DriveStatus::parse(s).expect(s);
            assert_eq!(st.as_str(), s);
        }
        assert!(DriveStatus::parse("bogus").is_none());
    }
}
