//! `borg pair` — device pairing CLI surface.
//!
//! Two modes:
//! - `borg pair show` — runs on the *daemon host*; calls `Pki.StartPairing`
//!   over the local UDS to mint a code and prints it.
//! - `borg pair <code> [--label X] [--socket /path/to/borgd-pair.sock]` —
//!   runs on the *client device*; drives the SPAKE2 exchange over the
//!   bootstrap UDS and persists the issued cert to the OS keychain.

use anyhow::{Context, Result};
use borgd::pairing::device::client as pair_client;
use identity::KekStore;
use qrcodegen::{QrCode, QrCodeEcc};
use std::path::PathBuf;

use crate::daemon_client::connect;

/// Keychain accounts where the client stashes its cert + key after pairing.
pub const CLIENT_CERT_KEY_ACCOUNT: &str = "client-cert-pem";
pub const CLIENT_KEY_KEY_ACCOUNT: &str = "client-key-pem";
pub const CA_CERT_ACCOUNT: &str = "client-ca-pem";

pub async fn run_show() -> Result<()> {
    let mut client = connect(None).await.context("connect to daemon")?;
    let resp = client
        .start_pairing("operator")
        .await
        .context("start pairing")?;
    let exp = chrono::DateTime::from_timestamp(resp.expires_at, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| resp.expires_at.to_string());
    println!("Pairing code: {}", resp.code);
    println!("Expires at:   {exp} (5 minutes)");
    println!();
    if let Ok(qr) = QrCode::encode_text(&resp.code, QrCodeEcc::Medium) {
        println!("{}", render_qr_ascii(&qr));
    }
    println!("On the client device, run:");
    println!("  borg pair {}", resp.code);
    Ok(())
}

/// Render a `QrCode` as a half-block ASCII string with a 1-module quiet zone.
///
/// Each terminal row encodes two QR rows via Unicode half-blocks, so the
/// rendered height is roughly `(size + 2) / 2` and the width matches the QR
/// size plus the quiet-zone border. Only the four characters `' '`, `'▀'`,
/// `'▄'`, and `'█'` (plus `'\n'`) appear in the output.
fn render_qr_ascii(qr: &QrCode) -> String {
    const QUIET: i32 = 1;
    let size = qr.size();
    let lo = -QUIET;
    let hi = size + QUIET;
    let module = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= size || y >= size {
            false
        } else {
            qr.get_module(x, y)
        }
    };
    let mut out = String::new();
    let mut y = lo;
    while y < hi {
        for x in lo..hi {
            let top = module(x, y);
            let bot = module(x, y + 1);
            out.push(match (top, bot) {
                (false, false) => ' ',
                (true, false) => '▀',
                (false, true) => '▄',
                (true, true) => '█',
            });
        }
        out.push('\n');
        y += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_qr_ascii_encodes_payload() {
        let qr = QrCode::encode_text("borg-pair-test-payload", QrCodeEcc::Medium).unwrap();
        let rendered = render_qr_ascii(&qr);
        assert!(!rendered.is_empty());
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(!lines.is_empty(), "rendered output must contain rows");
        // Width consistent across rows; only the four expected glyphs appear.
        let width = lines[0].chars().count();
        assert!(width > 0);
        for line in &lines {
            assert_eq!(line.chars().count(), width, "rows must be uniform width");
            for ch in line.chars() {
                assert!(
                    matches!(ch, ' ' | '▀' | '▄' | '█'),
                    "unexpected glyph {:?} in QR output",
                    ch,
                );
            }
        }
        // Two QR rows per terminal row + 1-module quiet zone on each side =>
        // height ≈ (size + 2) / 2, width = size + 2.
        let qr_size = qr.size() as usize;
        let expected_width = qr_size + 2;
        let expected_height = (qr_size + 2).div_ceil(2);
        assert_eq!(width, expected_width);
        assert_eq!(lines.len(), expected_height);
        // At least some "on" pixels — empty render would be all spaces.
        assert!(rendered.chars().any(|c| c != ' ' && c != '\n'));
    }
}

pub async fn run_complete(code: &str, device_label: &str, socket: Option<PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(default_pairing_socket_path);
    println!("Pairing with daemon at {} ...", socket_path.display());
    let result = pair_client::run(&socket_path, code, device_label)
        .await
        .context("pairing handshake")?;

    let kek = crate::daemon_client::keychain();
    kek.put(CA_CERT_ACCOUNT, result.ca_cert_pem.as_bytes())
        .context("stash CA cert in keychain")?;
    kek.put(CLIENT_CERT_KEY_ACCOUNT, result.client_cert_pem.as_bytes())
        .context("stash client cert in keychain")?;
    kek.put(CLIENT_KEY_KEY_ACCOUNT, result.client_key_pem.as_bytes())
        .context("stash client key in keychain")?;

    let exp = chrono::DateTime::from_timestamp(result.not_after, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| result.not_after.to_string());
    println!("Pairing complete.");
    println!("Cert valid until {exp}");
    Ok(())
}

fn default_pairing_socket_path() -> PathBuf {
    let home = cortex::config::Config::data_dir().unwrap_or_else(|_| PathBuf::from("."));
    cortex::paths::pairing_socket_path(&home)
}
