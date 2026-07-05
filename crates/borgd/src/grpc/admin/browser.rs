//! `BrowserManage` RPC body for the admin service (`super::AdminSvc`).

use super::*;

pub(super) async fn browser_manage(
    svc: &AdminSvc,
    req: Request<BrowserManageRequest>,
) -> Result<Response<BrowserManageResponse>, TStatus> {
    let req = req.into_inner();
    let action = req.action.trim().to_lowercase();
    let mut messages: Vec<String> = Vec::new();

    let resolver = open_settings_resolver().await?;

    // Helper: probe + return final BrowserManageResponse for the current setting.
    let read_status = |messages: Vec<String>| async {
        let cfg = cortex::config::Config::load_from_db()
            .await
            .map_err(map_internal("config load"))?;
        let url = cfg.browser.cdp_url.clone().unwrap_or_default();
        let connected = if url.is_empty() {
            false
        } else {
            tokio::task::spawn_blocking({
                let url = url.clone();
                move || match cortex::browser::cdp_attach::parse_cdp_url(&url) {
                    Ok(parsed) => cortex::browser::cdp_attach::is_cdp_ready_blocking(
                        &parsed,
                        std::time::Duration::from_secs(1),
                    ),
                    Err(_) => false,
                }
            })
            .await
            .unwrap_or(false)
        };
        Ok::<BrowserManageResponse, TStatus>(BrowserManageResponse {
            connected,
            url,
            messages,
        })
    };

    match action.as_str() {
        "status" | "" => {
            let resp = read_status(messages).await?;
            Ok(Response::new(resp))
        }
        "disconnect" => {
            let (_, source) = resolver
                .get_with_source("browser.cdp_url")
                .await
                .map_err(map_internal("settings get"))?;
            if matches!(source, cortex::settings::SettingSource::Database) {
                resolver
                    .unset("browser.cdp_url")
                    .await
                    .map_err(map_internal("settings unset"))?;
                notify_runtime_config_reload(&svc.config_notify_tx).await;
                messages.push("browser disconnected from CDP endpoint".to_string());
            } else {
                messages.push("browser is not connected (already in default mode)".to_string());
            }
            let resp = read_status(messages).await?;
            Ok(Response::new(resp))
        }
        "connect" => connect(svc, &resolver, &req.url, messages).await,
        other => Err(TStatus::invalid_argument(format!(
            "unknown browser action '{other}' (expected connect|disconnect|status)"
        ))),
    }
}

/// The `connect` action: validate the CDP URL, probe it (auto-launching a
/// debug Chromium when the default URL isn't reachable), then persist
/// `browser.cdp_url` and notify the runtime config reload.
async fn connect(
    svc: &AdminSvc,
    resolver: &SettingsResolver,
    url: &str,
    mut messages: Vec<String>,
) -> Result<Response<BrowserManageResponse>, TStatus> {
    let parsed = match cortex::browser::cdp_attach::parse_cdp_url(url) {
        Ok(p) => p,
        Err(e) => {
            messages.push(format!("⚠ {e}"));
            return Ok(Response::new(BrowserManageResponse {
                connected: false,
                url: String::new(),
                messages,
            }));
        }
    };

    messages.push(format!(
        "checking Chromium-family browser remote debugging at {}…",
        parsed.url
    ));

    let probe_url = parsed.clone();
    let mut reachable = tokio::task::spawn_blocking(move || {
        cortex::browser::cdp_attach::is_cdp_ready_blocking(
            &probe_url,
            std::time::Duration::from_millis(1000),
        )
    })
    .await
    .unwrap_or(false);

    // Auto-launch path: only attempted when the user is targeting
    // the default URL — we won't spawn a Chrome aimed at someone
    // else's port.
    if !reachable && parsed.url == cortex::browser::cdp_attach::DEFAULT_CDP_URL {
        messages.push(
            "Chromium-family browser isn't running with remote debugging — attempting to launch…"
                .to_string(),
        );
        let port = parsed.port;
        let launched = tokio::task::spawn_blocking(move || {
            cortex::browser::cdp_attach::try_launch_chrome_debug(port)
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn failed: {e}")));

        match launched {
            Ok(()) => {
                for _ in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let probe_url = parsed.clone();
                    let ok = tokio::task::spawn_blocking(move || {
                        cortex::browser::cdp_attach::is_cdp_ready_blocking(
                            &probe_url,
                            std::time::Duration::from_millis(500),
                        )
                    })
                    .await
                    .unwrap_or(false);
                    if ok {
                        reachable = true;
                        break;
                    }
                }
                if reachable {
                    messages.push(format!(
                        "✓ Chromium-family browser launched and listening on port {}",
                        parsed.port
                    ));
                    if let Ok(dir) = cortex::browser::cdp_attach::chrome_debug_data_dir() {
                        // Surfacing the profile dir tells the user this isn't
                        // their everyday Chrome — its cookies and history are
                        // isolated under ~/.borg/chrome-debug. Otherwise an
                        // operator could mistake the new window for their main
                        // browser and be surprised when logged-in sites aren't.
                        messages.push(format!("   Using isolated profile: {}", dir.display()));
                    }
                } else {
                    messages.push(format!(
                        "⚠ Browser launched but port {} isn't responding yet — retry /browser connect in a few seconds",
                        parsed.port
                    ));
                }
            }
            Err(e) => {
                messages.push(format!(
                    "⚠ Could not auto-launch a Chromium-family browser: {e}"
                ));
                if let Some(cmd) =
                    cortex::browser::cdp_attach::manual_chrome_debug_command(parsed.port)
                {
                    messages.push("Launch one manually:".to_string());
                    messages.push(format!("    {cmd}"));
                }
            }
        }
    }

    if !reachable {
        messages.push(format!(
            "browser not connected — port {} is not reachable at {}",
            parsed.port, parsed.url
        ));
        // For non-default URLs we skipped auto-launch (we won't
        // spawn a Chrome aimed at someone else's port). Still
        // emit the manual command so the user has an actionable
        // next step instead of just "not reachable".
        if parsed.url != cortex::browser::cdp_attach::DEFAULT_CDP_URL {
            if let Some(cmd) = cortex::browser::cdp_attach::manual_chrome_debug_command(parsed.port)
            {
                messages.push("Start a debug Chromium yourself with:".to_string());
                messages.push(format!("    {cmd}"));
            }
        }
        return Ok(Response::new(BrowserManageResponse {
            connected: false,
            url: String::new(),
            messages,
        }));
    }

    resolver
        .set("browser.cdp_url", &parsed.url)
        .await
        .map_err(map_invalid_arg("settings set"))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;

    // Be honest about what this RPC confirmed: a successful
    // /json/version probe + a persisted setting. The actual
    // chromiumoxide attach happens on the next browser tool
    // call; if it fails (rare — Chrome rejects WS upgrade,
    // process died between probe and use) the user gets the
    // actionable hint from `handle_browser` pointing at
    // `/browser disconnect`. Overclaiming "connected" here
    // would set the wrong expectation.
    messages.push(
        "🌐 CDP endpoint reachable — agent will attach on next browser tool call".to_string(),
    );
    messages.push(format!("   Endpoint: {}", parsed.url));

    Ok(Response::new(BrowserManageResponse {
        connected: true,
        url: parsed.url,
        messages,
    }))
}
