//! Typed configuration resource reads. Native shells never need service method strings.

use qaqh_config_api::resources::methods;
use qaqh_config_api::{ConfigCatalog, ConfigProfileRequest, HostConfigView, ProfileView};

use crate::{Client, Result};

impl Client {
    /// Read all profiles without changing the host default or attaching a business session.
    pub async fn config_catalog(&self) -> Result<ConfigCatalog> {
        self.service_v2(methods::CATALOG, serde_json::json!({}))
            .await
    }

    /// Read one named profile, including non-default model/wire/budget/capability settings.
    pub async fn config_profile(&self, profile_id: String) -> Result<ProfileView> {
        self.service_v2(methods::PROFILE, ConfigProfileRequest { profile_id })
            .await
    }

    /// Host configuration declarations, not runtime service health or actor applied state.
    pub async fn config_host(&self) -> Result<HostConfigView> {
        self.service_v2(methods::HOST, serde_json::json!({})).await
    }
}
