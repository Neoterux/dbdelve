//! The driver settings a profile may set, a closed list per engine.
//!
//! Each engine module maps a [`Property`] to the one driver field it names. There
//! is no passthrough of free keys: a free key could carry the read-only hold
//! (Postgres `options`, MySQL `init`), TLS settings or credentials, and every
//! one of those is set from a field that has its own gate.

use super::Engine;

/// A driver setting any engine may list in [`EngineSpec::driver_properties`].
///
/// [`EngineSpec::driver_properties`]: super::spec::EngineSpec::driver_properties
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    /// The name the server shows for this client.
    ApplicationName,
    /// How long dialing the server may take, in whole seconds.
    ConnectTimeout,
}

impl Property {
    /// The spelling in `profiles.toml`, matched exactly.
    pub fn key(self) -> &'static str {
        match self {
            Self::ApplicationName => "application_name",
            Self::ConnectTimeout => "connect_timeout",
        }
    }
}

/// The longest an application name may be. Postgres truncates at 63 bytes and
/// shows the rest nowhere, so a longer one only misleads.
const APPLICATION_NAME_CAP: usize = 64;

/// What a profile's driver properties say, typed. Part of the connection: a
/// change reconnects, and an agent's connection carries them as it carries the
/// host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Properties {
    pub application_name: Option<String>,
    /// Whole seconds, 1 to 600.
    pub connect_timeout: Option<u32>,
}

impl Properties {
    /// What `pairs` set on `engine`, or why they cannot. A key outside the
    /// engine's list is refused by name, and the whole list with it, so a
    /// profile never connects with part of what was written.
    pub fn parse(engine: Engine, pairs: &[(String, String)]) -> Result<Self, String> {
        let allowed = engine.spec().driver_properties;
        let mut properties = Self::default();
        let mut seen: Vec<Property> = Vec::new();
        for (key, value) in pairs {
            let Some(property) = allowed
                .iter()
                .copied()
                .find(|property| property.key() == key)
            else {
                return Err(format!(
                    "Unknown driver property `{key}` for {}.",
                    engine.label()
                ));
            };
            if seen.contains(&property) {
                return Err(format!("Driver property `{key}` is given twice."));
            }
            seen.push(property);
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("Driver property `{key}` has no value."));
            }
            match property {
                Property::ApplicationName => {
                    if value.chars().count() > APPLICATION_NAME_CAP {
                        return Err(format!(
                            "Driver property `{key}` is longer than {APPLICATION_NAME_CAP} characters."
                        ));
                    }
                    if value.chars().any(char::is_control) {
                        return Err(format!(
                            "Driver property `{key}` holds a control character."
                        ));
                    }
                    properties.application_name = Some(value.to_string());
                }
                Property::ConnectTimeout => {
                    let seconds = value
                        .parse::<u32>()
                        .ok()
                        .filter(|seconds| (1..=600).contains(seconds))
                        .ok_or_else(|| {
                            format!(
                                "Driver property `{key}` is `{value}`, not a whole number of \
                                 seconds from 1 to 600."
                            )
                        })?;
                    properties.connect_timeout = Some(seconds);
                }
            }
        }
        Ok(properties)
    }

    /// The set values as `profiles.toml` writes them, in the order an engine's
    /// list gives, whatever order they were written in, so saving an unchanged
    /// profile changes no byte.
    pub fn stored(&self) -> Vec<(String, String)> {
        [
            self.application_name
                .as_ref()
                .map(|name| (Property::ApplicationName, name.clone())),
            self.connect_timeout
                .map(|seconds| (Property::ConnectTimeout, seconds.to_string())),
        ]
        .into_iter()
        .flatten()
        .map(|(property, value)| (property.key().to_string(), value))
        .collect()
    }
}
