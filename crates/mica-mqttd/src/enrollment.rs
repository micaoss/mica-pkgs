//! Positive, package-owned enrollment for the MQTT application-data plane.
//!
//! Each file name in `/usr/lib/mica/mqtt-applications.d` is one exact D-Bus
//! service name. File contents are ignored. The application package that owns
//! that service also owns the exact D-Bus policy grant for `mica-mqttd`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};

use crate::topic::Application;

/// The management service is never an MQTT application-data source.
const MICAD_SERVICE: &str = "com.mica.micad";

/// The exact services admitted to MQTT.
#[derive(Debug, Clone, Default)]
pub struct Enrollment {
    applications: BTreeMap<String, Application>,
}

impl Enrollment {
    /// Build an enrollment from exact package-provided service names.
    ///
    /// # Errors
    pub fn from_names<I, S>(names: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut applications = BTreeMap::new();
        for name in names {
            let name = name.as_ref();
            if name == MICAD_SERVICE {
                bail!(
                    "{MICAD_SERVICE} is the local management service and cannot be enrolled in MQTT"
                );
            }
            let application = Application::from_enrollment(name)
                .with_context(|| format!("invalid MQTT application enrollment `{name}`"))?;
            applications.insert(name.to_string(), application);
        }
        Ok(Self { applications })
    }

    /// Load exact service names from the file names in `directory`.
    ///
    /// # Errors
    pub async fn load(directory: &Path) -> anyhow::Result<Self> {
        let mut entries = tokio::fs::read_dir(directory)
            .await
            .with_context(|| format!("read MQTT application directory {}", directory.display()))?;
        let mut names = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("read MQTT application directory {}", directory.display()))?
        {
            let file_type = entry
                .file_type()
                .await
                .with_context(|| format!("inspect {}", entry.path().display()))?;
            if !file_type.is_file() {
                bail!(
                    "MQTT application enrollment {} is not a regular file",
                    entry.path().display()
                );
            }
            let name = entry.file_name().into_string().map_err(|_| {
                anyhow::anyhow!(
                    "MQTT application enrollment {} has a non-UTF-8 file name",
                    entry.path().display()
                )
            })?;
            names.push(name);
        }
        Self::from_names(names)
    }

    /// Return the exact enrolled application named by `bus_name`.
    pub fn application(&self, bus_name: &str) -> Option<Application> {
        self.applications.get(bus_name).cloned()
    }
}
