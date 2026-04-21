/// Unified settings macro that generates both `apply_setting()` match arms
/// and `SETTING_REGISTRY` extractor entries from a single table.
///
/// Supported setter kinds:
///   - `string`            — assign `value.to_string()`
///   - `opt_string`        — `None` if empty, else `Some(value.to_string())`
///   - `parsed(T)`         — `value.parse::<T>()`
///   - `nonzero(T)`        — parsed + reject zero
///   - `range(T, min, max)`— parsed + range check
///   - `json`              — `serde_json::from_str(value)`
///   - `json_set`          — json, display as `(set)`
///   - `json_count(label)` — json, display as `(N label)`
///   - `json_quoted(err)`  — wraps value in quotes before json parse
///   - `readonly`          — only generates a registry extractor, no setter
///
/// Custom setters are handled outside the macro via a `custom` block.
#[doc(hidden)]
#[macro_export]
macro_rules! define_settings {
    (
        registry_and_apply {
            $( $key:literal => $($path:ident).+ , $kind:ident $( ( $($param:tt),* ) )? ; )*
        }

        // Entries that only appear in the registry (no apply_setting arm)
        registry_only {
            $( $ro_key:literal => $ro_extract:expr ; )*
        }

        // Custom apply_setting arms (complex validation that doesn't fit a macro kind)
        custom_apply {
            $( $ck:literal => |$self_id:ident, $key_id:ident, $val_id:ident| $custom_body:expr ; )*
        }

        // Custom registry extractors for custom_apply keys
        custom_extract {
            $( $ce_key:literal => $ce_extract:expr ; )*
        }

        // TUI popup metadata. Entries reference keys from any of the above
        // blocks; a unit test validates that every key here is known to
        // `SETTING_REGISTRY`. Lives alongside the registry so adding or
        // reordering a TUI setting is a one-file change.
        tui_settings {
            $(
                $tui_key:literal => $tui_label:literal , $tui_kind:ident , $tui_category:literal ;
            )*
        }
    ) => {
        impl $crate::config::Config {
            /// Apply a single key=value setting, returning a confirmation string.
            pub fn apply_setting(&mut self, key: &str, value: &str) -> ::anyhow::Result<String> {
                match key {
                    $(
                        $key => define_settings!(@apply self, key, value, $($path).+ , $kind $( ( $($param),* ) )? ),
                    )*
                    $(
                        $ck => {
                            let $self_id = self;
                            let $key_id = key;
                            let $val_id = value;
                            $custom_body
                        },
                    )*
                    // Dynamic skill entries (pattern-matched)
                    k if k.starts_with("skills.entries.") && k.ends_with(".enabled") => {
                        let name = k
                            .strip_prefix("skills.entries.")
                            .and_then(|s| s.strip_suffix(".enabled"))
                            .ok_or_else(|| ::anyhow::anyhow!("Invalid skill entry key: {k}"))?
                            .to_string();
                        let enabled = $crate::config::parse_value::<bool>(value, k)?;
                        if !enabled && $crate::skills::MANDATORY_SKILLS.contains(&name.as_str()) {
                            ::anyhow::bail!(
                                "Skill '{name}' is mandatory and cannot be disabled."
                            );
                        }
                        self.skills.entries.entry(name).or_default().enabled = enabled;
                        Ok(format!("{k} = {enabled}"))
                    }
                    _ => ::anyhow::bail!(
                        "Unknown setting: {key}\nAvailable: {}",
                        $crate::settings::ALL_SETTING_KEYS.join(", ")
                    ),
                }
            }
        }

        /// Single source of truth for setting keys and their config extractors.
        pub const SETTING_REGISTRY: &[(&str, $crate::settings::SettingExtractor)] = &[
            $(
                ($key, define_settings!(@extract $($path).+ , $kind $( ( $($param),* ) )? )),
            )*
            $(
                ($ro_key, $ro_extract),
            )*
            $(
                ($ce_key, $ce_extract),
            )*
        ];

        /// TUI-visible subset with display metadata (label, kind, category).
        /// Rendered in the `/settings` popup in declaration order.
        pub const TUI_SETTINGS: &[$crate::settings::TuiSettingDecl] = &[
            $(
                $crate::settings::TuiSettingDecl {
                    key: $tui_key,
                    label: $tui_label,
                    kind: $crate::settings::TuiSettingKind::$tui_kind,
                    category: $tui_category,
                },
            )*
        ];

        // ── setter arms ──

    };

    // ── @apply arms: generate the setter code for each kind ──

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , string) => {{
        $self.$($path).+ = $value.to_string();
        Ok(format!("{} = {}", $key, $value))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , opt_string) => {{
        $self.$($path).+ = if $value.is_empty() {
            None
        } else {
            Some($value.to_string())
        };
        Ok(format!("{} = {}", $key, $value))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , parsed($T:ty)) => {{
        $self.$($path).+ = $crate::config::parse_value::<$T>($value, $key)?;
        Ok(format!("{} = {}", $key, $self.$($path).+))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , nonzero($T:ty)) => {{
        $self.$($path).+ = $crate::config::parse_nonzero::<$T>($value, $key)?;
        Ok(format!("{} = {}", $key, $self.$($path).+))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , range($T:ty, $min:expr, $max:expr)) => {{
        $self.$($path).+ = $crate::config::parse_range($value, $key, $min, $max)?;
        Ok(format!("{} = {}", $key, $self.$($path).+))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , json) => {{
        $self.$($path).+ = ::serde_json::from_str($value)
            .with_context(|| format!("Invalid JSON for {}", $key))?;
        Ok(format!("{} = {}", $key, $value))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , json_set) => {{
        $self.$($path).+ = ::serde_json::from_str($value)
            .with_context(|| format!("Invalid JSON for {}", $key))?;
        Ok(format!("{} = (set)", $key))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , json_count($label:expr)) => {{
        $self.$($path).+ = ::serde_json::from_str($value)
            .with_context(|| format!("Invalid JSON for {}", $key))?;
        Ok(format!("{} = ({} {})", $key, $self.$($path).+.len(), $label))
    }};

    (@apply $self:ident, $key:ident, $value:ident, $($path:ident).+ , json_quoted($err:expr)) => {{
        $self.$($path).+ = ::serde_json::from_str(&format!("\"{}\"", $value))
            .with_context(|| format!("{}: {}", $err, $value))?;
        Ok(format!("{} = {}", $key, $value))
    }};

    // ── @extract arms: generate the registry extractor for each kind ──

    (@extract $($path:ident).+ , string) => {
        |c| c.$($path).+.clone()
    };

    (@extract $($path:ident).+ , opt_string) => {
        |c| c.$($path).+.clone().unwrap_or_default()
    };

    (@extract $($path:ident).+ , parsed($T:ty)) => {
        |c| format!("{}", c.$($path).+)
    };

    (@extract $($path:ident).+ , nonzero($T:ty)) => {
        |c| format!("{}", c.$($path).+)
    };

    (@extract $($path:ident).+ , range($T:ty, $min:expr, $max:expr)) => {
        |c| format!("{}", c.$($path).+)
    };

    (@extract $($path:ident).+ , json) => {
        |c| ::serde_json::to_string(&c.$($path).+).unwrap_or_default()
    };

    (@extract $($path:ident).+ , json_set) => {
        |c| ::serde_json::to_string(&c.$($path).+).unwrap_or_default()
    };

    (@extract $($path:ident).+ , json_count($label:expr)) => {
        |c| ::serde_json::to_string(&c.$($path).+).unwrap_or_default()
    };

    (@extract $($path:ident).+ , json_quoted($err:expr)) => {
        |c| ::serde_json::to_string(&c.$($path).+)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    };
}
