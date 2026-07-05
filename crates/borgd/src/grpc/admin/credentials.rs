//! Credential-pool, API-key-routing, and provider CRUD RPC bodies for the
//! admin service (`super::AdminSvc`).

use super::*;

pub(super) async fn list_credentials(
    req: Request<ListCredentialsRequest>,
) -> Result<Response<CredentialList>, TStatus> {
    let req = req.into_inner();
    let providers = async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let pool = build_primary_pool(&config, &db).await?;
        if !req.provider.is_empty() && !pool_provider_matches(&pool, &req.provider) {
            return anyhow::Ok(vec![]);
        }
        Ok(vec![credential_pool_to_proto(&config, &pool)])
    }
    .await
    .map_err(map_internal("list_credentials"))?;
    Ok(Response::new(CredentialList { providers }))
}

pub(super) async fn add_credential(
    svc: &AdminSvc,
    req: Request<AddCredentialRequest>,
) -> Result<Response<CredentialEntry>, TStatus> {
    let req = req.into_inner();
    let key = req.key.trim().to_string();
    if key.is_empty() {
        return Err(TStatus::invalid_argument("key must not be empty"));
    }
    let provider = req.provider;
    let label_in = req.label;
    let entry = async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let pool = build_primary_pool(&config, &db).await?;
        require_provider_match(&pool, &provider)?;
        let label = if label_in.trim().is_empty() {
            // Count live entries only — tombstones shouldn't bump the
            // default label counter ("manual-7" after one add post-remove
            // looks like a bug to operators).
            let live = pool
                .entries()
                .iter()
                .filter(|c| c.removed_at_ms.is_none())
                .count();
            format!("manual-{}", live + 1)
        } else {
            label_in.trim().to_string()
        };
        let cred = pool.add_entry(
            key,
            label,
            cortex::llm::credential_pool::CredentialSource::Manual,
        );
        anyhow::Ok(pooled_to_proto(pool.provider_str(), &cred))
    }
    .await
    .map_err(|e| TStatus::invalid_argument(format!("add_credential: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(entry))
}

pub(super) async fn remove_credential(
    svc: &AdminSvc,
    req: Request<RemoveCredentialRequest>,
) -> Result<Response<CredentialEntry>, TStatus> {
    let req = req.into_inner();
    if req.target.trim().is_empty() {
        return Err(TStatus::invalid_argument("target must not be empty"));
    }
    let entry = async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let pool = build_primary_pool(&config, &db).await?;
        require_provider_match(&pool, &req.provider)?;
        let (idx_zero, _existing) = pool
            .resolve_target(&req.target)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let removed = pool
            .remove_index(idx_zero + 1)
            .ok_or_else(|| anyhow::anyhow!("index out of range after resolve"))?;
        anyhow::Ok(pooled_to_proto(pool.provider_str(), &removed))
    }
    .await
    .map_err(|e| TStatus::invalid_argument(format!("remove_credential: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(entry))
}

pub(super) async fn reset_credentials(
    svc: &AdminSvc,
    req: Request<ResetCredentialsRequest>,
) -> Result<Response<ResetCredentialsResponse>, TStatus> {
    let req = req.into_inner();
    let cleared = async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let pool = build_primary_pool(&config, &db).await?;
        require_provider_match(&pool, &req.provider)?;
        anyhow::Ok(pool.reset_cooldowns() as u64)
    }
    .await
    .map_err(|e| TStatus::invalid_argument(format!("reset_credentials: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(ResetCredentialsResponse { cleared }))
}

pub(super) async fn pair_credential(
    svc: &AdminSvc,
    req: Request<PairCredentialRequest>,
) -> Result<Response<PairCredentialResponse>, TStatus> {
    let raw_kind = req.into_inner().kind.trim().to_ascii_lowercase();
    if raw_kind.is_empty() {
        return Err(TStatus::invalid_argument("kind must not be empty"));
    }
    // Normalize hyphenated forms (`claude-code`) to the discoverer's
    // canonical `name()` (`claude_code`) — the registry keys on underscores.
    let kind = raw_kind.replace('-', "_");
    let entries = async {
        use anyhow::Context;
        use cortex::llm::credential_pool::{
            is_safe_base_url, pool_load_config_for_provider, AuthType, CredentialPool,
            PoolLoadConfig,
        };
        use cortex::llm::credential_sources::discoverer_by_name;
        use cortex::llm::provider::Provider;

        let discoverer = discoverer_by_name(&kind).ok_or_else(|| {
            anyhow::anyhow!("unknown OAuth kind `{kind}` — try claude_code | codex | qwen_cli")
        })?;
        let provider = discoverer.provider();

        let db = Database::open().await?;
        let db_path = cortex::config::Config::db_path()?;
        let config = cortex::config::Config::load_from_db().await?;

        // Build a load config pointing at this OAuth provider. Pairing
        // seeds the pool regardless of whether the operator has switched
        // their primary to that flavor yet, so we bypass
        // `config.llm.provider`. Env-var seeding is disabled — pairing
        // is an explicit operator action, not an env scan.
        let mut load_cfg: PoolLoadConfig = pool_load_config_for_provider(
            provider,
            config.llm.credential_strategy,
            config.llm.credential_cooldown_secs,
            config.llm.max_concurrent_per_credential,
            vec![],
        );
        load_cfg.proactive_rate_limit_threshold_pct =
            config.llm.proactive_rate_limit_threshold_pct;
        load_cfg.proactive_rate_limit_min_window_secs =
            config.llm.proactive_rate_limit_min_window_secs;
        load_cfg.env_vars.clear();
        let pool = CredentialPool::load(&db, &db_path, provider, load_cfg).await?;

        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME for OAuth discovery"))?;
        let discovered = discoverer
            .discover(&home)
            .with_context(|| format!("running discoverer `{}`", discoverer.name()))?;

        let mut out = Vec::new();
        for disc in discovered {
            // Custom-endpoint sources carry a per-credential base_url
            // that needs SSRF-style validation before it lands in the
            // pool — operator-writable files can otherwise redirect
            // traffic on next request.
            if let Some(url) = disc.base_url.as_deref() {
                if !is_safe_base_url(url) {
                    anyhow::bail!(
                        "discoverer `{}` returned unsafe base_url `{}` — must be https or http+loopback",
                        discoverer.name(),
                        url
                    );
                }
            }
            let source = match provider {
                Provider::Custom => cortex::llm::credential_pool::CredentialSource::Custom {
                    name: discoverer.name().to_string(),
                    base_url: disc.base_url.clone().unwrap_or_default(),
                },
                _ => cortex::llm::credential_pool::CredentialSource::OAuth {
                    name: discoverer.name().to_string(),
                },
            };
            let auth_type = if disc.refresh_token.is_some() {
                AuthType::OAuth
            } else {
                AuthType::ApiKey
            };
            let cred = pool.add_entry_full(
                disc.access_token,
                disc.label,
                source,
                auth_type,
                disc.refresh_token,
                disc.expires_at_ms,
                disc.base_url,
                disc.extra_json,
            );
            out.push(pooled_to_proto(pool.provider_str(), &cred));
        }
        anyhow::Ok(out)
    }
    .await
    .map_err(|e| TStatus::invalid_argument(format!("pair_credential: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(PairCredentialResponse { entries }))
}

pub(super) async fn set_credential_strategy(
    svc: &AdminSvc,
    req: Request<SetCredentialStrategyRequest>,
) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    // Validate input early so we don't write garbage to settings.
    use std::str::FromStr;
    cortex::llm::credential_pool::CredentialStrategy::from_str(&req.strategy).map_err(|_| {
        TStatus::invalid_argument(format!(
            "unknown strategy `{}` — try fill_first, round_robin, least_used, random",
            req.strategy
        ))
    })?;
    let provider = req.provider;
    let strategy_value = req.strategy;
    async {
        let db = Database::open().await?;
        let config = cortex::config::Config::load_from_db().await?;
        let pool = build_primary_pool(&config, &db).await?;
        require_provider_match(&pool, &provider)?;
        // Persist via the settings registry so it's restart-stable, and
        // notify the runtime config reload so the live LlmClient picks
        // up the new strategy on its next rebuild.
        let resolver = cortex::settings::SettingsResolver::load().await?;
        // The `json_quoted` setter wraps the value itself before
        // deserializing — we pass the raw variant string.
        resolver
            .set("llm.credential_strategy", &strategy_value)
            .await
            .map_err(|e| anyhow::anyhow!("set llm.credential_strategy: {e}"))?;
        anyhow::Ok(())
    }
    .await
    .map_err(|e| TStatus::invalid_argument(format!("set_credential_strategy: {e}")))?;
    notify_runtime_config_reload(&svc.config_notify_tx).await;
    Ok(Response::new(Empty {}))
}

pub(super) async fn get_api_key(req: Request<ApiKeyRef>) -> Result<Response<ApiKeyValue>, TStatus> {
    let provider = req.into_inner().provider;
    if provider.is_empty() {
        return Err(TStatus::invalid_argument("provider required"));
    }
    let provider_for_lookup = provider.clone();
    let body = async {
        let db = Database::open().await?;
        let key = format!("credentials.{provider_for_lookup}");
        let raw = db.get_setting(&key).await?;
        match raw {
            Some(s) if !s.is_empty() => anyhow::Ok(Some(serde_json::from_str(&s)?)),
            _ => Ok(None),
        }
    }
    .await
    .map_err(map_internal("get_api_key"))?;

    let value = match body {
        None => ApiKeyValue {
            provider,
            body: Some(protocols::admin::api_key_value::Body::Unset(Empty {})),
        },
        Some(ApiKeyRouting::Literal { value }) => ApiKeyValue {
            provider,
            body: Some(protocols::admin::api_key_value::Body::Literal(value)),
        },
        Some(ApiKeyRouting::Keychain { service, account }) => ApiKeyValue {
            provider,
            body: Some(protocols::admin::api_key_value::Body::Keychain(
                KeychainRef { service, account },
            )),
        },
    };
    Ok(Response::new(value))
}

pub(super) async fn set_api_key(req: Request<ApiKeyValue>) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    if req.provider.is_empty() {
        return Err(TStatus::invalid_argument("provider required"));
    }
    let routing = match req.body {
        Some(protocols::admin::api_key_value::Body::Literal(v)) => {
            if v.is_empty() {
                return Err(TStatus::invalid_argument("literal value must be non-empty"));
            }
            ApiKeyRouting::Literal { value: v }
        }
        Some(protocols::admin::api_key_value::Body::Keychain(kr)) => {
            if kr.service.is_empty() || kr.account.is_empty() {
                return Err(TStatus::invalid_argument(
                    "keychain ref requires service and account",
                ));
            }
            ApiKeyRouting::Keychain {
                service: kr.service,
                account: kr.account,
            }
        }
        Some(protocols::admin::api_key_value::Body::Unset(_)) | None => {
            return Err(TStatus::invalid_argument(
                "set_api_key requires literal or keychain body; use unset_api_key to clear",
            ));
        }
    };
    let provider = req.provider;
    async {
        let db = Database::open().await?;
        let key = format!("credentials.{provider}");
        let json = serde_json::to_string(&routing)?;
        db.set_setting(&key, &json).await?;
        anyhow::Ok(())
    }
    .await
    .map_err(map_internal("set_api_key"))?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn unset_api_key(req: Request<ApiKeyRef>) -> Result<Response<Empty>, TStatus> {
    let provider = req.into_inner().provider;
    if provider.is_empty() {
        return Err(TStatus::invalid_argument("provider required"));
    }
    async {
        let db = Database::open().await?;
        let key = format!("credentials.{provider}");
        db.delete_setting(&key).await?;
        anyhow::Ok(())
    }
    .await
    .map_err(map_internal("unset_api_key"))?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn list_api_keys(_req: Request<Empty>) -> Result<Response<ApiKeyList>, TStatus> {
    let entries = async {
        let db = Database::open().await?;
        let rows = db.list_settings().await?;
        let mut out = Vec::new();
        for (key, value, _) in rows {
            if !key.starts_with("credentials.") {
                continue;
            }
            let provider = match key.strip_prefix("credentials.") {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => continue,
            };
            let backend = match serde_json::from_str::<ApiKeyRouting>(&value) {
                Ok(ApiKeyRouting::Literal { .. }) => "literal",
                Ok(ApiKeyRouting::Keychain { .. }) => "keychain",
                Err(e) => {
                    tracing::warn!("list_api_keys: unparseable credentials.{provider} JSON: {e}");
                    "unset"
                }
            };
            out.push(ApiKeyEntry {
                provider,
                backend: backend.to_string(),
            });
        }
        anyhow::Ok(out)
    }
    .await
    .map_err(map_internal("list_api_keys"))?;
    Ok(Response::new(ApiKeyList { entries }))
}

pub(super) async fn add_provider(
    req: Request<AddProviderRequest>,
) -> Result<Response<Empty>, TStatus> {
    let req = req.into_inner();
    if req.id.is_empty() || req.id.contains(char::is_whitespace) {
        return Err(TStatus::invalid_argument(
            "id must be non-empty and contain no whitespace",
        ));
    }
    let category = match req.category.to_ascii_lowercase().as_str() {
        "byo" | "byo_endpoint" => cortex::providers::ProviderCategory::BYOEndpoint,
        "local" => cortex::providers::ProviderCategory::Local,
        "public" | "public_cloud" => cortex::providers::ProviderCategory::PublicCloud,
        other => {
            return Err(TStatus::invalid_argument(format!(
                "unknown category '{other}'; expected byo, local, or public_cloud"
            )));
        }
    };
    let id_for_err = req.id.clone();
    async {
        let db = Database::open().await?;
        let display_name = if req.display_name.is_empty() {
            req.id.clone()
        } else {
            req.display_name
        };
        let api_key_env = if req.api_key_env.is_empty() {
            None
        } else {
            Some(req.api_key_env)
        };
        let row = cortex::providers::ProviderRow {
            id: req.id,
            category,
            display_name,
            endpoint_url: req.base_url,
            api_key_env,
            enabled: true,
        };
        row.insert(&db).await?;
        anyhow::Ok(())
    }
    .await
    .map_err(|e| TStatus::internal(format!("add_provider '{id_for_err}' failed: {e}")))?;
    Ok(Response::new(Empty {}))
}

pub(super) async fn remove_provider(
    req: Request<RemoveProviderRequest>,
) -> Result<Response<Empty>, TStatus> {
    let id = req.into_inner().id;
    if id.is_empty() {
        return Err(TStatus::invalid_argument("id must be non-empty"));
    }
    let id_for_err = id.clone();
    async {
        let db = Database::open().await?;
        // Idempotent — `false` (already absent) is a successful no-op.
        cortex::providers::ProviderRow::delete(&db, &id).await?;
        anyhow::Ok(())
    }
    .await
    .map_err(|e| TStatus::internal(format!("remove_provider '{id_for_err}' failed: {e}")))?;
    Ok(Response::new(Empty {}))
}
