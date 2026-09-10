// Stub — full implementation in Wave 2
// DatumCloudClient with External auth only

use crate::datum_cloud::external_token_source::ExternalTokenSource;

#[derive(Debug, Clone)]
pub struct DatumCloudClient;

impl DatumCloudClient {
    pub fn with_external_token_source(_source: ExternalTokenSource) -> Self {
        Self
    }

    pub fn is_plugin_mode(&self) -> bool {
        true
    }

    pub fn token(&self) -> Option<String> {
        None
    }

    pub fn api_url(&self) -> String {
        String::new()
    }
}
