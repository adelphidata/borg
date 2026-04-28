//! SPAKE2 pairing handshake over a plaintext bootstrap UDS socket.
//!
//! Wire protocol (length-prefixed frames, big-endian u32 length):
//!
//! ```text
//!   client → daemon : <code_plaintext bytes>          (frame 1)
//!   client → daemon : <SPAKE2 group element A>        (frame 2)
//!   daemon → client : <SPAKE2 group element B>        (frame 3)
//!   daemon → client : AEAD(JSON{"ca_cert_pem":...,"client_cert_pem":...})  (frame 4)
//!   client → daemon : AEAD(JSON{"client_pub_pem":...,"device_label":...}) (frame 5b? swap-order)
//! ```
//!
//! Concretely we structure the exchange as: client sends code + their public
//! key + label first (after PAKE), then daemon issues cert and sends back.
//! Plaintext code travels in the clear over UDS — that's the bootstrap
//! premise. PAKE proves the code without exposing it past the local socket.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use cortex::db::{audit_events, AuditWriter, ConsumeOutcome, Database};
use hkdf::Hkdf;
use identity::{
    ca::{issue_client_cert, ClientCertParams},
    fingerprint_sha256,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Identity strings bound into SPAKE2 — must match between daemon and client.
const ID_DAEMON: &[u8] = b"borg-pair-daemon";
const ID_CLIENT: &[u8] = b"borg-pair-client";
/// Validity for the freshly issued client cert.
const CLIENT_VALIDITY_DAYS: u32 = 90;

#[derive(Debug, Serialize, Deserialize)]
struct ClientHello {
    client_pub_pem: String,
    device_label: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonReply {
    ca_cert_pem: String,
    client_cert_pem: String,
    not_after: i64,
}

/// State the bootstrap listener needs to mint client certs.
#[derive(Clone)]
pub struct BootstrapState {
    /// Shared database handle for paired-device + cert writes.
    pub db: Arc<Mutex<Database>>,
    /// Audit writer for pairing-related events.
    pub audit: AuditWriter,
    /// Root CA certificate (PEM) returned to clients on success.
    pub ca_cert_pem: String,
    /// Root CA private key (PEM) used to sign issued client certs.
    pub ca_key_pem: String,
}

/// Plaintext UDS listener that runs only while ≥1 pairing code is active.
pub struct BootstrapListener {
    socket_path: PathBuf,
}

impl BootstrapListener {
    /// Construct a listener bound to `socket_path` on `serve`.
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Bind and serve. Drops the socket file when the future is dropped.
    pub async fn serve(self, state: BootstrapState) -> Result<()> {
        // Best-effort cleanup of stale socket.
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("bind bootstrap socket {:?}", self.socket_path))?;
        // Restrict to owner-only, like the main UDS.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(path = %self.socket_path.display(), "pairing bootstrap listener ready");

        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "bootstrap listener accept failed");
                    continue;
                }
            };
            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = run_pairing_handshake(stream, &st).await {
                    tracing::warn!(error = %e, "pairing handshake failed");
                }
            });
        }
    }
}

/// Drive a single pairing handshake on a connected stream. Public so tests
/// can invoke it without binding a real socket.
pub async fn run_pairing_handshake(mut stream: UnixStream, state: &BootstrapState) -> Result<()> {
    // Frame 1: client sends plaintext code.
    let code_bytes = read_frame(&mut stream).await.context("read code frame")?;
    let code = String::from_utf8(code_bytes).context("code not utf-8")?;

    // Validate code against DB before doing PAKE work — single-use semantics
    // are enforced via consume_pairing_code.
    let outcome = {
        let g = state
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.consume_pairing_code(&code)?
    };
    if outcome != ConsumeOutcome::Accepted {
        state.audit.emit(
            audit_events::PAIRING_DENIED,
            "system",
            &serde_json::json!({"reason": format!("{:?}", outcome)}),
        );
        return Err(anyhow!("pairing code rejected: {outcome:?}"));
    }

    // SPAKE2 — daemon side.
    let (daemon_state, daemon_msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &SpakeIdentity::new(ID_DAEMON),
        &SpakeIdentity::new(ID_CLIENT),
    );

    // Frame 2: client sends SPAKE2 message.
    let client_msg = read_frame(&mut stream).await.context("read spake msg")?;
    // Frame 3: daemon replies.
    write_frame(&mut stream, daemon_msg.as_slice())
        .await
        .context("send spake msg")?;

    let shared_key = daemon_state
        .finish(&client_msg)
        .map_err(|e| anyhow!("spake2 finish: {e:?}"))?;
    let aead = derive_aead(&shared_key)?;

    // Frame 4 (encrypted): client sends ClientHello.
    let nonce4 = read_frame(&mut stream).await.context("read nonce4")?;
    let ct4 = read_frame(&mut stream).await.context("read ct4")?;
    let pt4 = aead
        .decrypt(Nonce::from_slice(&nonce4), ct4.as_ref())
        .map_err(|e| anyhow!("decrypt client hello: {e:?}"))?;
    let hello: ClientHello = serde_json::from_slice(&pt4).context("parse client hello")?;

    // Issue client cert.
    let client_pubkey = parse_client_pub(&hello.client_pub_pem)?;
    let device_id = fingerprint_sha256(client_pubkey.as_bytes_for_id());
    let params = ClientCertParams {
        device_label: &hello.device_label,
        device_id: &device_id,
        client_public_key_pem: &hello.client_pub_pem,
        validity_days: CLIENT_VALIDITY_DAYS,
    };
    let issued = issue_client_cert(&state.ca_cert_pem, &state.ca_key_pem, &params)
        .context("issue client cert")?;
    let cert_fp = fingerprint_sha256(&issued.cert_der);

    // Persist cert + paired_device.
    {
        let g = state
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let serial = g.insert_internal_cert(
            "client",
            &issued.cert_pem,
            &cert_fp,
            &hello.device_label,
            issued.not_before,
            issued.not_after,
        )?;
        g.insert_paired_device(
            &cert_fp,
            &hello.device_label,
            "{}",
            &hello.device_label,
            serial,
        )?;
    }
    state.audit.emit(
        audit_events::CERT_ISSUED,
        "system",
        &serde_json::json!({"fingerprint": cert_fp, "device_label": hello.device_label}),
    );
    state.audit.emit(
        audit_events::PAIRING_APPROVED,
        "system",
        &serde_json::json!({"fingerprint": cert_fp, "device_label": hello.device_label}),
    );

    // Frame 5 (encrypted): daemon sends DaemonReply.
    let reply = DaemonReply {
        ca_cert_pem: state.ca_cert_pem.clone(),
        client_cert_pem: issued.cert_pem,
        not_after: issued.not_after,
    };
    let reply_pt = serde_json::to_vec(&reply)?;
    let nonce5 = random_nonce();
    let ct5 = aead
        .encrypt(Nonce::from_slice(&nonce5), reply_pt.as_ref())
        .map_err(|e| anyhow!("encrypt reply: {e:?}"))?;
    write_frame(&mut stream, &nonce5).await?;
    write_frame(&mut stream, &ct5).await?;

    Ok(())
}

/// Parsed client public key — wraps the rcgen `KeyPair`-derived bytes so we
/// can fingerprint it for the device id.
struct ClientPubBytes(Vec<u8>);
impl ClientPubBytes {
    fn as_bytes_for_id(&self) -> &[u8] {
        &self.0
    }
}

fn parse_client_pub(pem: &str) -> Result<ClientPubBytes> {
    let kp = rcgen::KeyPair::from_pem(pem).context("parse client pubkey pem")?;
    Ok(ClientPubBytes(kp.public_key_der()))
}

fn derive_aead(shared: &[u8]) -> Result<ChaCha20Poly1305> {
    let hk = Hkdf::<Sha256>::new(Some(b"borg-pair"), shared);
    let mut okm = [0u8; 32];
    hk.expand(b"aead-key", &mut okm)
        .map_err(|e| anyhow!("hkdf expand: {e}"))?;
    Ok(ChaCha20Poly1305::new(Key::from_slice(&okm)))
}

fn random_nonce() -> [u8; 12] {
    use rand::RngCore;
    let mut n = [0u8; 12];
    rand::rng().fill_bytes(&mut n);
    n
}

async fn read_frame(s: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n > 16 * 1024 {
        return Err(anyhow!("frame too large: {n}"));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame(s: &mut UnixStream, data: &[u8]) -> Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    s.write_all(&len).await?;
    s.write_all(data).await?;
    s.flush().await?;
    Ok(())
}

/// Public API used by `borg pair` CLI to drive the client side of the
/// handshake. Returns the issued client cert, ca cert, and the keypair the
/// client generated locally (PEM).
pub mod client {
    use super::*;

    /// Result of a successful pairing.
    pub struct PairResult {
        /// Root CA certificate (PEM) the client should trust.
        pub ca_cert_pem: String,
        /// Newly minted client certificate (PEM).
        pub client_cert_pem: String,
        /// Locally generated client private key (PEM).
        pub client_key_pem: String,
        /// Unix-seconds expiry of the issued client cert.
        pub not_after: i64,
    }

    /// Drive the client side of the pairing handshake against `socket_path`.
    pub async fn run(
        socket_path: &std::path::Path,
        code: &str,
        device_label: &str,
    ) -> Result<PairResult> {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connect bootstrap socket {socket_path:?}"))?;

        // Frame 1: send code.
        write_frame(&mut stream, code.as_bytes()).await?;

        // SPAKE2 — client side.
        let (client_state, client_msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &SpakeIdentity::new(ID_DAEMON),
            &SpakeIdentity::new(ID_CLIENT),
        );

        // Frame 2: send our SPAKE2 msg.
        write_frame(&mut stream, client_msg.as_slice()).await?;
        // Frame 3: receive daemon's.
        let daemon_msg = read_frame(&mut stream).await?;
        let shared_key = client_state
            .finish(&daemon_msg)
            .map_err(|e| anyhow!("spake2 finish: {e:?}"))?;
        let aead = derive_aead(&shared_key)?;

        // Generate our client keypair.
        let client_kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| anyhow!("client keypair: {e}"))?;
        let client_pem = client_kp.serialize_pem();

        // Frame 4: send client hello.
        let hello = ClientHello {
            client_pub_pem: client_pem.clone(),
            device_label: device_label.to_string(),
        };
        let pt = serde_json::to_vec(&hello)?;
        let nonce = random_nonce();
        let ct = aead
            .encrypt(Nonce::from_slice(&nonce), pt.as_ref())
            .map_err(|e| anyhow!("encrypt hello: {e:?}"))?;
        write_frame(&mut stream, &nonce).await?;
        write_frame(&mut stream, &ct).await?;

        // Frame 5: receive daemon reply.
        let nonce5 = read_frame(&mut stream).await?;
        let ct5 = read_frame(&mut stream).await?;
        let pt5 = aead
            .decrypt(Nonce::from_slice(&nonce5), ct5.as_ref())
            .map_err(|e| anyhow!("decrypt reply: {e:?}"))?;
        let reply: DaemonReply = serde_json::from_slice(&pt5)?;

        Ok(PairResult {
            ca_cert_pem: reply.ca_cert_pem,
            client_cert_pem: reply.client_cert_pem,
            client_key_pem: client_pem,
            not_after: reply.not_after,
        })
    }
}

/// Mint a fresh 8-char base32 pairing code (Crockford-flavored alphabet, no
/// confusable chars).
pub fn mint_pairing_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut rng = rand::rng();
    (0..9)
        .map(|i| {
            if i == 4 {
                '-'
            } else {
                let idx = rng.random_range(0..ALPHABET.len());
                ALPHABET[idx] as char
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::pki_grpc::PAIRING_INTENT;
    use identity::{ca::generate_root_ca, MemoryKekStore};
    use rusqlite::Connection;

    fn make_state() -> (BootstrapState, MemoryKekStore) {
        let kek = MemoryKekStore::new();
        let db =
            Database::from_connection(Connection::open_in_memory().expect("db")).expect("init");
        let ca = generate_root_ca("borg-test-ca").expect("ca");
        let shared = Arc::new(Mutex::new(db));
        let audit = AuditWriter::new(shared.clone());
        (
            BootstrapState {
                db: shared,
                audit,
                ca_cert_pem: ca.cert_pem,
                ca_key_pem: ca.key_pkcs8_pem,
            },
            kek,
        )
    }

    #[test]
    fn mint_pairing_code_format_8_chars_with_dash() {
        for _ in 0..50 {
            let code = mint_pairing_code();
            // Length 9 ("XXXX-XXXX"), dash at index 4.
            assert_eq!(code.len(), 9, "code len: {code}");
            assert_eq!(code.chars().nth(4), Some('-'));
            let alphabet: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
            for (i, c) in code.chars().enumerate() {
                if i == 4 {
                    continue;
                }
                assert!(
                    alphabet.contains(&(c as u8)),
                    "code {code} contains illegal char {c}"
                );
            }
        }
    }

    #[tokio::test]
    async fn happy_path_pairing_yields_paired_device_and_audit_events() {
        let (state, _kek) = make_state();
        let code = mint_pairing_code();
        {
            let g = state.db.lock().expect("lock");
            g.create_pairing_code(&code, PAIRING_INTENT, 300)
                .expect("mint");
        }
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("pair.sock");
        let listener_state = state.clone();
        let listener_path = path.clone();
        let server = tokio::spawn(async move {
            BootstrapListener::new(listener_path)
                .serve(listener_state)
                .await
        });

        // Wait for socket to exist.
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let result = client::run(&path, &code, "alice-laptop")
            .await
            .expect("client pairing");
        assert!(result.client_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(result.ca_cert_pem.contains("BEGIN CERTIFICATE"));

        // Verify state in DB.
        let devices = state
            .db
            .lock()
            .expect("lock")
            .list_paired_devices()
            .expect("list");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_label, "alice-laptop");

        // Audit chain has cert_issued + pairing_approved.
        let events = state
            .db
            .lock()
            .expect("lock")
            .list_audit_events()
            .expect("audit");
        assert!(events
            .iter()
            .any(|e| e.event_type == audit_events::CERT_ISSUED));
        assert!(events
            .iter()
            .any(|e| e.event_type == audit_events::PAIRING_APPROVED));

        server.abort();
    }

    #[tokio::test]
    async fn wrong_code_rejected_and_audited_as_denied() {
        let (state, _kek) = make_state();
        // Mint code A; client tries code B.
        {
            let g = state.db.lock().expect("lock");
            g.create_pairing_code("CORRECT-1", PAIRING_INTENT, 300)
                .expect("mint");
        }
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("pair.sock");
        let listener_state = state.clone();
        let listener_path = path.clone();
        let server = tokio::spawn(async move {
            BootstrapListener::new(listener_path)
                .serve(listener_state)
                .await
        });
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let result = client::run(&path, "WRONG-99", "intruder").await;
        assert!(result.is_err(), "wrong code should fail");

        let events = state
            .db
            .lock()
            .expect("lock")
            .list_audit_events()
            .expect("audit");
        assert!(events
            .iter()
            .any(|e| e.event_type == audit_events::PAIRING_DENIED));
        // No cert issued.
        assert!(events
            .iter()
            .all(|e| e.event_type != audit_events::CERT_ISSUED));

        server.abort();
    }
}
