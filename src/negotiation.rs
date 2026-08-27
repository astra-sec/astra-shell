use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};

use crate::protocol::{CapabilityOffer, CapabilitySelection, ClientHello, ServerHello};
use crate::{LEGACY_CLIENT_HELLO_VERSION, MINIMUM_PROTOCOL_VERSION, PROTOCOL_VERSION};

pub const MAX_CAPABILITIES: usize = 64;
pub const MAX_CAPABILITY_NAME_BYTES: usize = 64;

pub const CAPABILITY_LEGACY_ANSI_SNAPSHOT: &str = "terminal.legacy_ansi_snapshot";
pub const CAPABILITY_SEMANTIC_STATE: &str = "terminal.semantic_state";
pub const CAPABILITY_HISTORY_PAGING: &str = "terminal.history_paging";
pub const CAPABILITY_DATAGRAM_STATE: &str = "terminal.datagram_state";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityRange {
    pub name: &'static str,
    pub minimum_version: u32,
    pub maximum_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolSupport {
    pub minimum_version: u32,
    pub maximum_version: u32,
    pub capabilities: Vec<CapabilityRange>,
}

impl ProtocolSupport {
    pub fn runtime() -> Self {
        Self {
            minimum_version: MINIMUM_PROTOCOL_VERSION,
            maximum_version: PROTOCOL_VERSION,
            capabilities: vec![CapabilityRange {
                name: CAPABILITY_LEGACY_ANSI_SNAPSHOT,
                minimum_version: 1,
                maximum_version: 1,
            }],
        }
    }

    #[cfg(test)]
    fn with_future_capabilities() -> Self {
        Self {
            minimum_version: MINIMUM_PROTOCOL_VERSION,
            maximum_version: PROTOCOL_VERSION,
            capabilities: vec![
                CapabilityRange {
                    name: CAPABILITY_SEMANTIC_STATE,
                    minimum_version: 1,
                    maximum_version: 2,
                },
                CapabilityRange {
                    name: CAPABILITY_HISTORY_PAGING,
                    minimum_version: 1,
                    maximum_version: 1,
                },
                CapabilityRange {
                    name: CAPABILITY_DATAGRAM_STATE,
                    minimum_version: 1,
                    maximum_version: 1,
                },
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedProtocol {
    pub version: u32,
    pub capabilities: BTreeMap<String, u32>,
}

impl NegotiatedProtocol {
    pub fn has(&self, name: &str, minimum_version: u32) -> bool {
        self.capabilities
            .get(name)
            .is_some_and(|version| *version >= minimum_version)
    }
}

pub fn client_hello(username: impl Into<String>, support: &ProtocolSupport) -> ClientHello {
    ClientHello {
        // Old astra/1 servers require exact equality here. Keeping the legacy
        // hint at one lets them accept this hello and ignore the appended range.
        protocol_version: LEGACY_CLIENT_HELLO_VERSION,
        username: username.into(),
        minimum_protocol_version: support.minimum_version,
        maximum_protocol_version: support.maximum_version,
        capabilities: support
            .capabilities
            .iter()
            .map(|capability| CapabilityOffer {
                name: capability.name.to_owned(),
                minimum_version: capability.minimum_version,
                maximum_version: capability.maximum_version,
            })
            .collect(),
    }
}

pub fn negotiate_client_hello(
    hello: &ClientHello,
    server: &ProtocolSupport,
) -> Result<NegotiatedProtocol> {
    validate_support(server)?;
    let (client_minimum, client_maximum) = hello_version_range(hello)?;
    let minimum = client_minimum.max(server.minimum_version);
    let maximum = client_maximum.min(server.maximum_version);
    ensure!(
        minimum <= maximum,
        "client protocol range {client_minimum}..={client_maximum} does not overlap server range {}..={}",
        server.minimum_version,
        server.maximum_version
    );

    let client_capabilities = validate_offers(&hello.capabilities)?;
    let mut capabilities = BTreeMap::new();
    for supported in &server.capabilities {
        let Some(offered) = client_capabilities.get(supported.name) else {
            continue;
        };
        let capability_minimum = offered.0.max(supported.minimum_version);
        let capability_maximum = offered.1.min(supported.maximum_version);
        if capability_minimum <= capability_maximum {
            capabilities.insert(supported.name.to_owned(), capability_maximum);
        }
    }

    Ok(NegotiatedProtocol {
        version: maximum,
        capabilities,
    })
}

pub fn selections(negotiated: &NegotiatedProtocol) -> Vec<CapabilitySelection> {
    negotiated
        .capabilities
        .iter()
        .map(|(name, version)| CapabilitySelection {
            name: name.clone(),
            version: *version,
        })
        .collect()
}

pub fn validate_server_hello(
    client: &ClientHello,
    server: &ServerHello,
) -> Result<NegotiatedProtocol> {
    let (minimum, maximum) = hello_version_range(client)?;
    ensure!(
        (minimum..=maximum).contains(&server.protocol_version),
        "server selected protocol version {} outside client range {minimum}..={maximum}",
        server.protocol_version
    );
    let offers = validate_offers(&client.capabilities)?;
    ensure!(
        server.capabilities.len() <= MAX_CAPABILITIES,
        "server selected too many capabilities"
    );
    let mut capabilities = BTreeMap::new();
    for selected in &server.capabilities {
        validate_capability_name(&selected.name)?;
        ensure!(selected.version > 0, "selected capability version is zero");
        let Some((offered_minimum, offered_maximum)) = offers.get(selected.name.as_str()) else {
            anyhow::bail!("server selected unoffered capability {}", selected.name);
        };
        ensure!(
            (*offered_minimum..=*offered_maximum).contains(&selected.version),
            "server selected unsupported version {} for capability {}",
            selected.version,
            selected.name
        );
        ensure!(
            capabilities
                .insert(selected.name.clone(), selected.version)
                .is_none(),
            "server selected capability {} more than once",
            selected.name
        );
    }
    Ok(NegotiatedProtocol {
        version: server.protocol_version,
        capabilities,
    })
}

fn hello_version_range(hello: &ClientHello) -> Result<(u32, u32)> {
    if hello.minimum_protocol_version == 0 && hello.maximum_protocol_version == 0 {
        ensure!(
            hello.protocol_version > 0,
            "legacy client protocol version is zero"
        );
        return Ok((hello.protocol_version, hello.protocol_version));
    }
    ensure!(
        hello.minimum_protocol_version > 0
            && hello.minimum_protocol_version <= hello.maximum_protocol_version,
        "client protocol version range is invalid"
    );
    ensure!(
        (hello.minimum_protocol_version..=hello.maximum_protocol_version)
            .contains(&hello.protocol_version),
        "legacy protocol hint is outside the advertised client range"
    );
    Ok((
        hello.minimum_protocol_version,
        hello.maximum_protocol_version,
    ))
}

fn validate_support(support: &ProtocolSupport) -> Result<()> {
    ensure!(
        support.minimum_version > 0 && support.minimum_version <= support.maximum_version,
        "server protocol support range is invalid"
    );
    ensure!(
        support.capabilities.len() <= MAX_CAPABILITIES,
        "server supports too many capabilities"
    );
    let mut names = BTreeSet::new();
    for capability in &support.capabilities {
        validate_capability_name(capability.name)?;
        ensure!(
            capability.minimum_version > 0
                && capability.minimum_version <= capability.maximum_version,
            "server capability {} has an invalid version range",
            capability.name
        );
        ensure!(
            names.insert(capability.name),
            "server capability {} is duplicated",
            capability.name
        );
    }
    Ok(())
}

fn validate_offers(offers: &[CapabilityOffer]) -> Result<BTreeMap<&str, (u32, u32)>> {
    ensure!(
        offers.len() <= MAX_CAPABILITIES,
        "client offered too many capabilities"
    );
    let mut result = BTreeMap::new();
    for offer in offers {
        validate_capability_name(&offer.name)?;
        ensure!(
            offer.minimum_version > 0 && offer.minimum_version <= offer.maximum_version,
            "client capability {} has an invalid version range",
            offer.name
        );
        ensure!(
            result
                .insert(
                    offer.name.as_str(),
                    (offer.minimum_version, offer.maximum_version)
                )
                .is_none(),
            "client capability {} is duplicated",
            offer.name
        );
    }
    Ok(result)
}

fn validate_capability_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_CAPABILITY_NAME_BYTES
            && name.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_')),
        "invalid protocol capability name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn new_peers_select_current_version_and_capability_intersection() {
        let support = ProtocolSupport::with_future_capabilities();
        let hello = client_hello("astra", &support);
        let negotiated = negotiate_client_hello(&hello, &support).unwrap();
        assert_eq!(negotiated.version, PROTOCOL_VERSION);
        assert!(negotiated.has(CAPABILITY_SEMANTIC_STATE, 2));
        assert!(negotiated.has(CAPABILITY_HISTORY_PAGING, 1));
        assert!(negotiated.has(CAPABILITY_DATAGRAM_STATE, 1));

        let server_hello = ServerHello {
            protocol_version: negotiated.version,
            challenge: vec![],
            server_instance: String::new(),
            capabilities: selections(&negotiated),
        };
        assert_eq!(
            validate_server_hello(&hello, &server_hello).unwrap(),
            negotiated
        );
    }

    #[test]
    fn new_client_hello_remains_accepted_by_an_n_minus_one_decoder() {
        let hello = client_hello("legacy", &ProtocolSupport::runtime());
        let encoded = hello.encode_to_vec();

        #[derive(Clone, PartialEq, Message)]
        struct LegacyClientHello {
            #[prost(uint32, tag = "1")]
            protocol_version: u32,
            #[prost(string, tag = "2")]
            username: String,
        }

        let legacy = LegacyClientHello::decode(encoded.as_slice()).unwrap();
        assert_eq!(legacy.protocol_version, LEGACY_CLIENT_HELLO_VERSION);
        assert_eq!(legacy.username, "legacy");
    }

    #[test]
    fn old_client_and_server_negotiate_n_minus_one_without_capabilities() {
        let old_client = ClientHello {
            protocol_version: MINIMUM_PROTOCOL_VERSION,
            username: "old".into(),
            minimum_protocol_version: 0,
            maximum_protocol_version: 0,
            capabilities: vec![],
        };
        let negotiated = negotiate_client_hello(&old_client, &ProtocolSupport::runtime()).unwrap();
        assert_eq!(negotiated.version, MINIMUM_PROTOCOL_VERSION);
        assert!(negotiated.capabilities.is_empty());

        let new_client = client_hello("new", &ProtocolSupport::runtime());
        let old_server = ServerHello {
            protocol_version: MINIMUM_PROTOCOL_VERSION,
            challenge: vec![],
            server_instance: String::new(),
            capabilities: vec![],
        };
        let negotiated = validate_server_hello(&new_client, &old_server).unwrap();
        assert_eq!(negotiated.version, MINIMUM_PROTOCOL_VERSION);
        assert!(negotiated.capabilities.is_empty());
    }

    #[test]
    fn rejects_non_overlapping_or_invented_negotiation() {
        let mut hello = client_hello("new", &ProtocolSupport::runtime());
        hello.minimum_protocol_version = PROTOCOL_VERSION + 1;
        hello.maximum_protocol_version = PROTOCOL_VERSION + 1;
        hello.protocol_version = PROTOCOL_VERSION + 1;
        assert!(negotiate_client_hello(&hello, &ProtocolSupport::runtime()).is_err());

        let hello = client_hello("new", &ProtocolSupport::runtime());
        let server = ServerHello {
            protocol_version: PROTOCOL_VERSION,
            challenge: vec![],
            server_instance: String::new(),
            capabilities: vec![CapabilitySelection {
                name: CAPABILITY_DATAGRAM_STATE.into(),
                version: 1,
            }],
        };
        assert!(validate_server_hello(&hello, &server).is_err());
    }
}
