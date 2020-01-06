// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

//! Client and node implementations for a resilient decentralised network.
//!
//! The network is based on the [`kademlia_routing_table`][1] and uses the XOR metric to define the
//! "distance" between two [`XorName`][2]s. `XorName`s are used as addresses of nodes, clients as
//! well as data.
//!
//! [1]: ../kademlia_routing_table/index.html
//! [2]: ../xor_name/struct.XorName.html
//!
//! Messages are exchanged between _authorities_, where an `Authority` can be an individual client
//! or node, or a collection of nodes called a "section", or a subset of a section called a "group".
//! In all cases, messages are cryptographically signed by the sender, and in the case of sections
//! and groups, it is verified that a sufficient number of members agree on the message: only if
//! that quorum is reached, the message is delivered. In addition, each message has a unique ID, and
//! is delivered only once.
//!
//! Section and group authorities are also addressed using a single `XorName`. The members are the
//! nodes that are closest to that name. Sections contain a minimum number of nodes with the minimum
//! value specified as a network-wide constant. Groups are of fixed size, defined as the above
//! minimum section size. Since nodes are assigned their name by the network, this provides
//! redundancy and resilience: a node has no control over which section or group authority it will
//! be a member of, and without a majority in the section or group it cannot forge a message from
//! there.
//!
//! The library also provides different types for the messages' data.
//!
//!
//! # Usage
//!
//! `Node` is used to handle and send requests within that network, and to implement its
//! functionality, e.g. storing and retrieving data, validating permissions, managing metadata, etc.
//!
//! # Sequence diagrams
//!
//! - [Bootstrapping](bootstrap.png)
//! - [Churn (`NewNode`)](new-node.png)

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/maidsafe/QA/master/Images/maidsafe_logo.png",
    html_favicon_url = "https://maidsafe.net/img/favicon.ico",
    test(attr(forbid(warnings)))
)]
// For explanation of lint checks, run `rustc -W help` or see
// https://github.com/maidsafe/QA/blob/master/Documentation/Rust%20Lint%20Checks.md
#![forbid(
    exceeding_bitshifts,
    mutable_transmutes,
    no_mangle_const_items,
    unknown_crate_types,
    warnings
)]
#![deny(
    bad_style,
    improper_ctypes,
    missing_docs,
    non_shorthand_field_patterns,
    overflowing_literals,
    plugin_as_library,
    stable_features,
    unconditional_recursion,
    unknown_lints,
    unsafe_code,
    unused,
    unused_allocation,
    unused_attributes,
    unused_comparisons,
    unused_features,
    unused_parens,
    while_true,
    clippy::unicode_not_nfc,
    clippy::wrong_pub_self_convention,
    clippy::option_unwrap_used
)]
#![warn(
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_results
)]
// FIXME: move `deprecated` to `deny` section above
#![allow(
    box_pointers,
    deprecated,
    missing_copy_implementations,
    missing_debug_implementations,
    variant_size_differences,
    non_camel_case_types,
    // FIXME: allow `needless_pass_by_value` until it's OK to change the public API
    // FIXME: Re-enable `redundant_field_names`.
    clippy::needless_pass_by_value,
    clippy::redundant_field_names
)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;

// Needs to be before all other modules to make the macros available to them.
#[macro_use]
mod macros;

mod action;
mod authority;
mod chain;
mod error;
mod event;
mod event_stream;
mod id;
mod message_filter;
mod messages;
mod network_service;
mod node;
mod outbox;
mod parsec;
mod pause;
mod peer_map;
mod relocation;
mod routing_message_filter;
mod signature_accumulator;
mod state_machine;
mod states;
mod time;
mod timer;
mod utils;
mod xor_space;

/// Mocking utilities.
#[cfg(feature = "mock_base")]
pub mod mock;

// Random number generation
#[cfg(not(feature = "mock_base"))]
mod rng;
#[cfg(feature = "mock_base")]
pub mod rng;

// Cryptography
#[cfg(not(feature = "mock_base"))]
mod crypto;
#[cfg(feature = "mock_base")]
use self::mock::crypto;

// Networking layer
#[cfg(feature = "mock_base")]
use self::mock::quic_p2p;
#[cfg(not(feature = "mock_base"))]
use quic_p2p;

pub use {
    self::{
        authority::Authority,
        chain::quorum_count,
        error::{InterfaceError, RoutingError},
        event::{ClientEvent, ConnectEvent, Event},
        event_stream::EventStream,
        id::{FullId, P2pNode, PublicId},
        node::{Node, NodeBuilder},
        pause::PausedState,
        quic_p2p::{Config as NetworkConfig, NodeInfo as ConnectionInfo},
        utils::XorTargetInterval,
        xor_space::{Prefix, XorName, XorNameFromHexError, Xorable, XOR_NAME_BITS, XOR_NAME_LEN},
    },
    threshold_crypto::{
        PublicKey as BlsPublicKey, PublicKeySet as BlsPublicKeySet,
        PublicKeyShare as BlsPublicKeyShare, SecretKeySet as BlsSecretKeySet,
        SecretKeyShare as BlsSecretKeyShare, Signature as BlsSignature,
        SignatureShare as BlsSignatureShare,
    },
};

#[cfg(feature = "mock_base")]
pub use self::{
    chain::{
        delivery_group_size, elders_info_for_test, section_proof_chain_from_elders_info,
        NetworkParams, SectionKeyShare, MIN_AGE,
    },
    messages::{HopMessage, Message, MessageContent, RoutingMessage, SignedRoutingMessage},
    parsec::generate_bls_threshold_secret_key,
    relocation::Overrides as RelocationOverrides,
};

#[cfg(feature = "mock_base")]
#[doc(hidden)]
pub mod test_consts {
    pub use crate::{
        chain::{UNRESPONSIVE_THRESHOLD, UNRESPONSIVE_WINDOW},
        states::{BOOTSTRAP_TIMEOUT, JOIN_TIMEOUT},
    };
}

/// Quorum is defined as having strictly greater than `QUORUM_NUMERATOR / QUORUM_DENOMINATOR`
/// agreement; using only integer arithmetic a quorum can be checked with
/// `votes * QUORUM_DENOMINATOR > voters * QUORUM_NUMERATOR`.
pub const QUORUM_NUMERATOR: usize = 2;
/// See `QUORUM_NUMERATOR`.
pub const QUORUM_DENOMINATOR: usize = 3;

/// Default minimal section size.
pub const MIN_SECTION_SIZE: usize = 3;

/// Minimal safe section size. Routing will keep adding nodes until the section reaches this size.
/// More nodes might be added if requested by the upper layers.
/// This number also detemines when split happens - if both post-split sections would have at least
/// this number of nodes.
pub const SAFE_SECTION_SIZE: usize = 100;

/// Number of elders per section.
pub const ELDER_SIZE: usize = 7;

use self::quic_p2p::Event as NetworkEvent;
#[cfg(any(test, feature = "mock_base"))]
use unwrap::unwrap;

// Format that can be sent between peers
#[cfg(not(feature = "mock_base"))]
type NetworkBytes = bytes::Bytes;
#[cfg(feature = "mock_base")]
type NetworkBytes = std::rc::Rc<Message>;

#[cfg(test)]
mod tests {
    use super::{QUORUM_DENOMINATOR, QUORUM_NUMERATOR};

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn quorum_check() {
        assert!(
            QUORUM_NUMERATOR < QUORUM_DENOMINATOR,
            "Quorum impossible to achieve"
        );
        assert!(
            QUORUM_NUMERATOR * 2 >= QUORUM_DENOMINATOR,
            "Quorum does not guarantee agreement"
        );
    }
}
