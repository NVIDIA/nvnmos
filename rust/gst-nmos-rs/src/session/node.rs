// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared Node identity / network-services settings for `OpenSession` and
//! `AddNode` (the `NodeConfig` protobuf message).

use gstreamer as gst;
use nvnmos_rpc::v1::{AssetConfig, NetworkServicesConfig, NodeConfig};
use thiserror::Error;

use crate::CAT;
use crate::network_services::parse_nmos_url;

/// Snapshot of element properties that map to proto `NodeConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NodeSettings {
    pub node_seed: String,
    pub http_port: u16,
    pub host_name: String,
    pub node_properties: Option<gst::Structure>,
    pub domain: String,
    pub registration_url: String,
    pub system_url: String,
}

#[derive(Debug, Error)]
pub(crate) enum NodePropertiesError {
    #[error("unknown node-properties field `{0}`")]
    UnknownField(String),
    #[error("node-properties field `{field}` must be {expected}")]
    InvalidType {
        field: &'static str,
        expected: &'static str,
    },
    #[error(
        "node-properties asset information requires non-empty `manufacturer`, \
         `product`, `instance-id`, and `functions` fields"
    )]
    IncompleteAsset,
}

impl NodeSettings {
    /// Build the `NodeConfig` for `OpenSession` or `AddNode`.
    ///
    /// Invalid `registration-url` / `system-url` values are logged and omitted;
    /// other fields are still forwarded.
    pub(crate) fn to_node_config(&self) -> Result<NodeConfig, NodePropertiesError> {
        let mut config = NodeConfig {
            seed: self.node_seed.clone(),
            host_name: self.host_name.clone(),
            http_port: u32::from(self.http_port),
            network_services: self.build_network_services(),
            ..NodeConfig::default()
        };

        if let Some(properties) = self.node_properties.as_ref() {
            apply_node_properties(properties, &mut config)?;
        }

        Ok(config)
    }

    fn build_network_services(&self) -> Option<NetworkServicesConfig> {
        let mut has_network_services = false;
        let mut network_services = NetworkServicesConfig::default();

        if !self.domain.is_empty() {
            network_services.domain = self.domain.clone();
            has_network_services = true;
        }

        if !self.registration_url.is_empty() {
            let reg_parts = parse_nmos_url(&self.registration_url, "registration");
            if reg_parts.valid {
                network_services.registration_address = reg_parts.address;
                network_services.registration_port = reg_parts.port;
                if !reg_parts.version.is_empty() {
                    network_services.registration_version = reg_parts.version;
                }
                has_network_services = true;
            } else {
                gst::warning!(
                    CAT,
                    "failed to parse registration-url `{}`; property ignored",
                    self.registration_url
                );
            }
        }

        if !self.system_url.is_empty() {
            let sys_parts = parse_nmos_url(&self.system_url, "system");
            if sys_parts.valid {
                network_services.system_address = sys_parts.address;
                network_services.system_port = sys_parts.port;
                if !sys_parts.version.is_empty() {
                    network_services.system_version = sys_parts.version;
                }
                has_network_services = true;
            } else {
                gst::warning!(
                    CAT,
                    "failed to parse system-url `{}`; property ignored",
                    self.system_url
                );
            }
        }

        if has_network_services {
            Some(network_services)
        } else {
            None
        }
    }
}

fn apply_node_properties(
    properties: &gst::StructureRef,
    config: &mut NodeConfig,
) -> Result<(), NodePropertiesError> {
    const FIELDS: &[&str] = &[
        "label",
        "description",
        "manufacturer",
        "product",
        "instance-id",
        "functions",
    ];

    for field in properties.fields() {
        if !FIELDS.contains(&field.as_str()) {
            return Err(NodePropertiesError::UnknownField(field.to_string()));
        }
    }

    config.label = optional_string(properties, "label")?.unwrap_or_default();
    config.description = optional_string(properties, "description")?.unwrap_or_default();

    let manufacturer = optional_string(properties, "manufacturer")?;
    let product = optional_string(properties, "product")?;
    let instance_id = optional_string(properties, "instance-id")?;
    let functions = optional_functions(properties)?;
    let has_asset_field =
        manufacturer.is_some() || product.is_some() || instance_id.is_some() || functions.is_some();

    if has_asset_field {
        let asset = AssetConfig {
            manufacturer: manufacturer.unwrap_or_default(),
            product: product.unwrap_or_default(),
            instance_id: instance_id.unwrap_or_default(),
            functions: functions.unwrap_or_default(),
        };
        if asset.manufacturer.is_empty()
            || asset.product.is_empty()
            || asset.instance_id.is_empty()
            || asset.functions.is_empty()
            || asset.functions.iter().any(String::is_empty)
        {
            return Err(NodePropertiesError::IncompleteAsset);
        }
        config.asset_tags = Some(asset);
    }

    Ok(())
}

fn optional_string(
    properties: &gst::StructureRef,
    field: &'static str,
) -> Result<Option<String>, NodePropertiesError> {
    if !properties.has_field(field) {
        return Ok(None);
    }
    properties
        .get::<String>(field)
        .map(Some)
        .map_err(|_| NodePropertiesError::InvalidType {
            field,
            expected: "a string",
        })
}

fn optional_functions(
    properties: &gst::StructureRef,
) -> Result<Option<Vec<String>>, NodePropertiesError> {
    const FIELD: &str = "functions";
    if !properties.has_field(FIELD) {
        return Ok(None);
    }
    if let Ok(function) = properties.get::<String>(FIELD) {
        return Ok(Some(vec![function]));
    }
    if let Ok(functions) = properties.get::<gst::Array>(FIELD) {
        return functions
            .iter()
            .map(|value| {
                value
                    .get::<String>()
                    .map_err(|_| NodePropertiesError::InvalidType {
                        field: FIELD,
                        expected: "a string or an array of strings",
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some);
    }
    if let Ok(functions) = properties.get::<gst::List>(FIELD) {
        return functions
            .iter()
            .map(|value| {
                value
                    .get::<String>()
                    .map_err(|_| NodePropertiesError::InvalidType {
                        field: FIELD,
                        expected: "a string or an array of strings",
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some);
    }
    Err(NodePropertiesError::InvalidType {
        field: FIELD,
        expected: "a string or an array of strings",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(properties: gst::Structure) -> NodeSettings {
        NodeSettings {
            node_seed: "node-a".to_owned(),
            node_properties: Some(properties),
            ..NodeSettings::default()
        }
    }

    #[test]
    fn node_properties_forwards_label_and_description() {
        crate::test_support::init_gst();
        let properties = gst::Structure::builder("properties")
            .field("label", "Studio encoder")
            .field("description", "Primary UHD encoder")
            .build();
        let config = settings(properties).to_node_config().unwrap();
        assert_eq!(config.label, "Studio encoder");
        assert_eq!(config.description, "Primary UHD encoder");
        assert!(config.asset_tags.is_none());
    }

    #[test]
    fn node_properties_forwards_asset_information() {
        crate::test_support::init_gst();
        let functions = gst::Array::new([
            "Meow".to_owned(),
            "Purr".to_owned(),
            "Hiss".to_owned(),
            "Yowl".to_owned(),
        ]);
        let properties = gst::Structure::builder("properties")
            .field("manufacturer", "Felis")
            .field("product", "catus")
            .field("instance-id", "Puss")
            .field("functions", functions)
            .build();
        let config = settings(properties).to_node_config().unwrap();
        let asset = config.asset_tags.expect("asset tags");
        assert_eq!(asset.manufacturer, "Felis");
        assert_eq!(asset.product, "catus");
        assert_eq!(asset.instance_id, "Puss");
        assert_eq!(asset.functions, ["Meow", "Purr", "Hiss", "Yowl"]);
    }

    #[test]
    fn node_properties_accepts_single_function_string() {
        crate::test_support::init_gst();
        let properties = gst::Structure::builder("properties")
            .field("manufacturer", "Felis")
            .field("product", "catus")
            .field("instance-id", "Puss")
            .field("functions", "Meow")
            .build();
        let config = settings(properties).to_node_config().unwrap();
        assert_eq!(config.asset_tags.expect("asset tags").functions, ["Meow"]);
    }

    #[test]
    fn node_properties_accepts_documented_structure_syntax() {
        crate::test_support::init_gst();
        let properties = "properties,label=Studio-A,description=Primary-encoder,\
                          manufacturer=Acme,product=(string)\"Widget Pro\",\
                          instance-id=XYZ123-456789,functions=(string)<Encoder,Decoder>"
            .parse::<gst::Structure>()
            .unwrap();
        let functions = properties
            .get::<gst::Array>("functions")
            .expect("functions should be a GstValueArray");
        assert_eq!(functions.len(), 2);
        assert_eq!(
            functions
                .iter()
                .map(|value| value.get::<String>().unwrap())
                .collect::<Vec<_>>(),
            ["Encoder", "Decoder"]
        );
        let config = settings(properties).to_node_config().unwrap();
        assert_eq!(config.label, "Studio-A");
        assert_eq!(
            config.asset_tags.expect("asset tags").functions,
            ["Encoder", "Decoder"]
        );
    }

    #[test]
    fn node_properties_rejects_partial_asset_information() {
        crate::test_support::init_gst();
        let properties = gst::Structure::builder("properties")
            .field("manufacturer", "NVIDIA")
            .build();
        assert!(matches!(
            settings(properties).to_node_config(),
            Err(NodePropertiesError::IncompleteAsset)
        ));
    }

    #[test]
    fn node_properties_rejects_unknown_fields() {
        crate::test_support::init_gst();
        let properties = gst::Structure::builder("properties")
            .field("lable", "typo")
            .build();
        assert!(matches!(
            settings(properties).to_node_config(),
            Err(NodePropertiesError::UnknownField(field)) if field == "lable"
        ));
    }
}
