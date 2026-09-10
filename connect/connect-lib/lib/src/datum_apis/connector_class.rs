use kube::CustomResource;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize)]
#[kube(
    group = "networking.datumapis.com",
    version = "v1alpha1",
    kind = "ConnectorClass",
    plural = "connectorclasses",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorClassSpec {}
